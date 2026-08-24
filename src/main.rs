use actix_cors::Cors;
use actix_files as fs;
use actix_web::middleware::DefaultHeaders;
use actix_web::{web, App, HttpResponse, HttpServer, Result};
use futures::stream::StreamExt;
use log::info;
use pj::*;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

async fn handle_list_chats(pool: web::Data<DbPool>) -> Result<HttpResponse> {
    let pool = pool.get_ref().clone();
    let chats = web::block(move || list_chats(&pool))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(chats))
}

async fn handle_create_chat(
    pool: web::Data<DbPool>,
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let pool = pool.get_ref().clone();
    let chat = web::block(move || create_chat(&pool))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let _ = event_tx.send(ChatChange::Upsert { id: chat.id });
    Ok(HttpResponse::Ok().json(chat))
}

async fn handle_delete_chat(
    path: web::Path<i64>,
    pool: web::Data<DbPool>,
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let id = path.into_inner();
    let pool = pool.get_ref().clone();
    web::block(move || delete_chat(&pool, id))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let _ = event_tx.send(ChatChange::Deleted { id });
    Ok(HttpResponse::Ok().json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct WorkdirRequest {
    workdir: Option<String>,
}

async fn handle_set_workdir(
    path: web::Path<i64>,
    req: web::Json<WorkdirRequest>,
    pool: web::Data<DbPool>,
) -> Result<HttpResponse> {
    let chat_id = path.into_inner();
    if let Some(dir) = req.workdir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        let p = std::path::Path::new(dir);
        if !p.is_absolute() {
            return Ok(HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "workdir must be an absolute path"})));
        }
        if !p.is_dir() {
            return Ok(HttpResponse::BadRequest()
                .json(serde_json::json!({"error": format!("directory does not exist: {dir}")})));
        }
    }
    let pool = pool.get_ref().clone();
    let dir = req.workdir.clone();
    web::block(move || set_chat_workdir(&pool, chat_id, dir.as_deref()))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({"ok": true, "workdir": req.workdir})))
}

async fn handle_get_messages(path: web::Path<i64>, pool: web::Data<DbPool>) -> Result<HttpResponse> {
    let id = path.into_inner();
    let pool = pool.get_ref().clone();
    let messages = web::block(move || get_messages(&pool, id))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(messages))
}

async fn handle_chat(
    req: web::Json<ChatRequest>,
    pool: web::Data<DbPool>,
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let ollama_url = ollama_url();
    let model = model_name();
    let message = req.message.clone();
    let chat_id = req.chat_id;
    let pool = pool.get_ref().clone();

    info!("Processing chat request for chat_id={:?}, message={}", chat_id, message);

    let chat_id = if let Some(id) = chat_id {
        let exists =
            chat_exists(&pool, id).map_err(actix_web::error::ErrorInternalServerError)?;
        if !exists {
            return Err(actix_web::error::ErrorBadRequest(format!(
                "chat {id} not found"
            )));
        }
        id
    } else {
        let pool = pool.clone();
        let title: String = message.chars().take(50).collect();
        web::block(move || {
            let conn = pool.get().unwrap();
            conn.execute("INSERT INTO chats (title) VALUES (?1)", params![title])?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(|e: rusqlite::Error| actix_web::error::ErrorInternalServerError(e))?
    };

    let pool_clone = pool.clone();
    let msg = message.clone();
    web::block(move || update_title_from_message(&pool_clone, chat_id, &msg))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let _ = event_tx.send(ChatChange::Upsert { id: chat_id });

    let pool_c = pool.clone();
    let msg_c = message.clone();
    web::block(move || add_message(&pool_c, chat_id, "user", &msg_c, None))
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("DB: {e}")))?
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let _ = event_tx.send(ChatChange::Activity {
        id: chat_id,
        state: ChatActivityState::Thinking,
    });

    let client = shared_ollama_http_client();
    let request = OllamaRequest {
        model,
        prompt: message,
    };

    let response = client
        .post(format!("{}/api/generate", ollama_url))
        .json(&request)
        .send()
        .await;

    match response {
        Ok(resp) => {
            let (tx, rx) = mpsc::unbounded_channel::<web::Bytes>();
            let pool2 = pool.clone();
            let chat_id2 = chat_id;
            let event_tx2 = event_tx.get_ref().clone();

            tokio::spawn(async move {
                let mut stream = Box::pin(resp.bytes_stream());
                let mut full_response = String::new();

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            let lines: Vec<&str> = text
                                .split('\n')
                                .filter(|l| !l.trim().is_empty())
                                .collect();

                            let mut result = String::new();
                            for line in &lines {
                                if let Ok(data) = serde_json::from_str::<OllamaChunk>(line) {
                                    let cleaned = data
                                        .response
                                        .replace("<think>", "")
                                        .replace("</think>", "");
                                    result.push_str(&cleaned);
                                }
                            }
                            full_response.push_str(&result);
                            let _ = tx.send(web::Bytes::from(result));
                        }
                        Err(e) => {
                            log::error!("Stream error: {}", e);
                            let _ = tx.send(web::Bytes::from(format!("Error: {e}")));
                        }
                    }
                }

                if !full_response.is_empty() {
                    tokio::task::spawn_blocking(move || {
                        add_message(&pool2, chat_id2, "assistant", &full_response, None).ok();
                    })
                    .await
                    .ok();
                    let _ = event_tx2.send(ChatChange::Upsert { id: chat_id2 });
                }
                let _ = event_tx2.send(ChatChange::Activity {
                    id: chat_id2,
                    state: ChatActivityState::Idle,
                });
            });

            Ok(HttpResponse::Ok()
                .insert_header(("X-Chat-Id", chat_id.to_string()))
                .content_type("text/plain; charset=utf-8")
                .streaming(
                    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
                        .map(Ok::<_, std::io::Error>),
                ))
        }
        Err(e) => {
            log::error!("Failed to connect to Ollama: {}", e);
            let _ = event_tx.send(ChatChange::Activity {
                id: chat_id,
                state: ChatActivityState::Idle,
            });
            Ok(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to connect to Ollama: {e}")
            })))
        }
    }
}

// ── Tool-enabled chat (shared agent engine) ──

#[derive(Deserialize)]
pub struct ToolChatRequest {
    pub chat_id: Option<i64>,
    pub message: String,
    pub images: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct ToolChatResponse {
    pub r#type: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<tools::ToolCall>>,
    pub chat_id: i64,
}

#[derive(Deserialize)]
pub struct ToolConfirmRequest {
    pub chat_id: i64,
    pub approved: bool,
}

fn ensure_chat(pool: &DbPool, chat_id: Option<i64>, first_message: &str) -> Result<i64> {
    match chat_id {
        Some(id) => {
            let exists = chat_exists(pool, id).map_err(actix_web::error::ErrorInternalServerError)?;
            if exists {
                Ok(id)
            } else {
                Err(actix_web::error::ErrorBadRequest(format!(
                    "chat {id} not found"
                )))
            }
        }
        None => create_chat(pool)
            .map_err(actix_web::error::ErrorInternalServerError)
            .map(|c| {
                let _ = update_title_from_message(pool, c.id, first_message);
                c.id
            }),
    }
}

fn turn_response(chat_id: i64, outcome: TurnOutcome) -> HttpResponse {
    match outcome {
        TurnOutcome::Reply(text) => HttpResponse::Ok().json(ToolChatResponse {
            r#type: "text".to_string(),
            content: text,
            tool_calls: None,
            chat_id,
        }),
        TurnOutcome::PendingTools(calls) => HttpResponse::Ok().json(ToolChatResponse {
            r#type: "tool_calls".to_string(),
            content: String::new(),
            tool_calls: Some(calls),
            chat_id,
        }),
    }
}

async fn handle_tool_chat(
    req: web::Json<ToolChatRequest>,
    pool: web::Data<DbPool>,
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let ollama_url = ollama_url();
    let model = model_name();
    let message = req.message.clone();
    let images = req.images.clone();
    let pool = pool.get_ref().clone();

    let has_images = images.as_ref().is_some_and(|i| !i.is_empty());
    let vision = model_supports_vision(&ollama_url, &model).await;
    log::info!("handle_tool_chat: model={}, has_images={}, supports_vision={}", model, has_images, vision);

    if has_images && !vision {
        let chat_id = ensure_chat(&pool, req.chat_id, &message)?;
        add_message(&pool, chat_id, "user", &message, None)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let reply = format!(
            "I can't read images — the current model '{}' doesn't support vision. \
             Switch to a vision-capable model (e.g. llava, bakllava, llava-phi3) to send images.",
            model
        );
        add_message(&pool, chat_id, "assistant", &reply, None)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let _ = event_tx.send(ChatChange::Upsert { id: chat_id });
        return Ok(HttpResponse::Ok().json(ToolChatResponse {
            r#type: "text".to_string(),
            content: reply,
            tool_calls: None,
            chat_id,
        }));
    }

    let chat_id = ensure_chat(&pool, req.chat_id, &message)?;
    let _ = event_tx.send(ChatChange::Upsert { id: chat_id });

    add_message(&pool, chat_id, "user", &message, images.as_deref())
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let _ = event_tx.send(ChatChange::Activity {
        id: chat_id,
        state: ChatActivityState::Thinking,
    });
    let _ = event_tx.send(ChatChange::Upsert { id: chat_id });

    match run_chat_turn(&pool, &ollama_url, &model, chat_id).await {
        Ok(outcome) => Ok(turn_response(chat_id, outcome)),
        Err(e) => {
            log::error!("Ollama error: {}", e);
            let _ = event_tx.send(ChatChange::Activity {
                id: chat_id,
                state: ChatActivityState::Idle,
            });
            Ok(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Ollama error: {e}")
            })))
        }
    }
}

async fn handle_tool_confirm(
    req: web::Json<ToolConfirmRequest>,
    pool: web::Data<DbPool>,
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let ollama_url = ollama_url();
    let model = model_name();
    let pool = pool.get_ref().clone();
    let chat_id = req.chat_id;

    let _ = event_tx.send(ChatChange::Activity {
        id: chat_id,
        state: ChatActivityState::Thinking,
    });

    match resolve_pending_tools(&pool, &ollama_url, &model, chat_id, req.approved).await {
        Ok(outcome) => Ok(turn_response(chat_id, outcome)),
        Err(e) => {
            log::error!("Tool confirm error: {}", e);
            let _ = event_tx.send(ChatChange::Activity {
                id: chat_id,
                state: ChatActivityState::Idle,
            });
            Ok(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("{e}")
            })))
        }
    }
}

async fn handle_get_pending_tools(
    path: web::Path<i64>,
    pool: web::Data<DbPool>,
) -> Result<HttpResponse> {
    let chat_id = path.into_inner();
    let pending =
        load_pending_tools(pool.get_ref(), chat_id).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({ "tool_calls": pending })))
}

#[derive(Deserialize)]
struct WriteFileRequest {
    path: String,
    content: String,
}

async fn handle_write_file(req: web::Json<WriteFileRequest>) -> Result<HttpResponse> {
    let path = &req.path;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            actix_web::error::ErrorInternalServerError(format!("mkdir error: {e}"))
        })?;
    }
    std::fs::write(path, &req.content).map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("write error: {e}"))
    })?;
    Ok(HttpResponse::Ok().json(serde_json::json!({"ok": true, "path": path, "bytes": req.content.len()})))
}

async fn handle_events(
    event_tx: web::Data<broadcast::Sender<ChatChange>>,
) -> Result<HttpResponse> {
    let mut rx = event_tx.subscribe();
    let (tx, rx_stream) = mpsc::unbounded_channel::<web::Bytes>();

    tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
        let _ = tx.send(web::Bytes::from("event: connected\ndata: {}\n\n"));
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if tx.send(web::Bytes::from("event: ping\ndata: {}\n\n")).is_err() {
                        break;
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(change) => {
                            let payload = serde_json::to_string(&change).unwrap_or_default();
                            let event = format!("data: {payload}\n\n");
                            if tx.send(web::Bytes::from(event)).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
    });

    Ok(HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("Connection", "keep-alive"))
        .streaming(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx_stream)
                .map(Ok::<_, std::io::Error>),
        ))
}

async fn health() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(serde_json::json!({"status": "ok"})))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let port = get_env_or("PORT", "8080").parse::<u16>().unwrap_or(8080);
    let database_url = database_url();

    let pool = create_pool(&database_url);
    init_db(&pool);

    let event_tx = start_event_server();
    info!("Event socket at {}", socket_path().display());

    let model = model_name();
    info!("Model: {model}");
    info!("Starting server on http://0.0.0.0:{}", port);

    let pool_data = web::Data::new(pool);
    let event_tx_data = web::Data::new(event_tx);

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        App::new()
            .app_data(pool_data.clone())
            .app_data(event_tx_data.clone())
            .wrap(cors)
            .wrap(DefaultHeaders::new().add(("Cache-Control", "no-store")))
            .route("/api/chat", web::post().to(handle_chat))
            .route("/api/chat/tools", web::post().to(handle_tool_chat))
            .route("/api/chat/tools/confirm", web::post().to(handle_tool_confirm))
            .route("/api/chats", web::post().to(handle_create_chat))
            .route("/api/chats", web::get().to(handle_list_chats))
            .route("/api/chats/{id}", web::delete().to(handle_delete_chat))
            .route("/api/chats/{id}/messages", web::get().to(handle_get_messages))
            .route("/api/chats/{id}/workdir", web::put().to(handle_set_workdir))
            .route("/api/chats/{id}/pending-tools", web::get().to(handle_get_pending_tools))
            .route("/api/events", web::get().to(handle_events))
            .route("/api/write-file", web::post().to(handle_write_file))
            .route("/health", web::get().to(health))
            .service(fs::Files::new("/", "./static").index_file("index.html"))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
