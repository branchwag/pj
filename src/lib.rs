use futures::stream::StreamExt;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::sync::broadcast;

pub mod tools;
use tools::ToolCall;

pub type DbPool = Pool<SqliteConnectionManager>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ChatActivityState {
    Idle,
    Thinking,
    AwaitingToolConfirmation,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ChatChange {
    Upsert { id: i64 },
    Deleted { id: i64 },
    Activity { id: i64, state: ChatActivityState },
}

pub fn default_database_url() -> String {
    format!("{}/data/chat.db", env!("CARGO_MANIFEST_DIR"))
}

pub fn database_url() -> String {
    env::var("DATABASE_URL").unwrap_or_else(|_| default_database_url())
}

fn model_presets() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            "speed",
            "gpt-oss:20b-cloud",
            "Fast free cloud model with solid tool calling",
        ),
        (
            "balanced",
            "gpt-oss:120b-cloud",
            "Strong open coding model on Ollama's free cloud tier (default)",
        ),
        (
            "quality",
            "minimax-m3:cloud",
            "Agentic cloud model with 1M token context",
        ),
        ("local-speed", "qwen2.5:1.5b", "Local, fully offline, fastest"),
        ("local-balanced", "qwen2.5:3b", "Local, offline, balanced"),
        ("local-quality", "qwen2.5:7b", "Local, offline, best quality"),
    ]
}

pub fn resolve_model() -> String {
    if let Ok(name) = env::var("MODEL_NAME") {
        return name;
    }
    let preset = get_env_or("MODEL_PRESET", "balanced");
    for (key, model, _) in model_presets() {
        if *key == preset {
            return model.to_string();
        }
    }
    eprintln!("Unknown MODEL_PRESET '{preset}', using balanced default");
    "gpt-oss:120b-cloud".to_string()
}

pub fn model_name() -> String {
    resolve_model()
}

pub fn ollama_url() -> String {
    get_env_or("OLLAMA_URL", "http://localhost:11434")
}

pub fn socket_path() -> PathBuf {
    let db_url = database_url();
    let mut p = PathBuf::from(&db_url);
    p.pop();
    p.push("events.sock");
    p
}

pub fn start_event_server() -> broadcast::Sender<ChatChange> {
    let (tx, _) = broadcast::channel::<ChatChange>(64);
    let tx2 = tx.clone();
    let path = socket_path();

    tokio::spawn(async move {
        let _ = std::fs::remove_file(&path);
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("event socket bind failed: {e}");
                return;
            }
        };
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut rx = tx2.subscribe();
            let tx3 = tx2.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                use tokio::io::AsyncWriteExt;
                let (reader, mut writer) = tokio::io::split(stream);
                let writer_task = tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(change) => {
                                let mut msg =
                                    serde_json::to_string(&change).unwrap_or_default();
                                msg.push('\n');
                                if writer.write_all(msg.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                });

                let mut reader = tokio::io::BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            if let Ok(change) = serde_json::from_str::<ChatChange>(line.trim_end())
                            {
                                let _ = tx3.send(change);
                            }
                        }
                        Err(_) => break,
                    }
                }
                writer_task.abort();
            });
        }
    });

    tx
}

pub fn publish_chat_change(change: &ChatChange) -> Result<(), String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("Failed to connect to event socket {}: {e}", path.display()))?;
    let mut msg = serde_json::to_string(change).map_err(|e| format!("Serialize event: {e}"))?;
    msg.push('\n');
    stream
        .write_all(msg.as_bytes())
        .map_err(|e| format!("Failed to publish event: {e}"))?;
    Ok(())
}

pub fn socket_inode(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            m.ino()
        }
        #[cfg(not(unix))]
        {
            0
        }
    })
}

pub struct EventClient {
    stream: UnixStream,
    buf: Vec<u8>,
    pub ino: u64,
}

impl EventClient {
    pub fn connect(path: &std::path::Path) -> Option<Self> {
        let ino = socket_inode(path)?;
        UnixStream::connect(path).ok().map(|s| {
            s.set_nonblocking(true).ok();
            Self {
                stream: s,
                buf: Vec::with_capacity(1024),
                ino,
            }
        })
    }

    pub fn try_recv(&mut self) -> Option<Option<ChatChange>> {
        let mut tmp = [0u8; 1024];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = self.buf.drain(..=pos).collect();
                        return Some(serde_json::from_slice(&line[..line.len() - 1]).ok());
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Some(None),
                Err(_) => return None,
            }
        }
    }
}

#[derive(Deserialize)]
pub struct ChatRequest {
    pub chat_id: Option<i64>,
    pub message: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct OllamaChunk {
    pub response: String,
    pub done: bool,
}

#[derive(Serialize)]
pub struct OllamaRequest {
    pub model: String,
    pub prompt: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatSummary {
    pub id: i64,
    pub title: String,
    pub created_at: String,
    pub message_count: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MessageOut {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

pub fn create_pool(database_url: &str) -> DbPool {
    if let Some(parent) = std::path::Path::new(database_url).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok();
    }
    let manager = SqliteConnectionManager::file(database_url);
    Pool::builder()
        .max_size(5)
        .build(manager)
        .expect("Failed to create DB pool")
}

pub fn init_db(pool: &DbPool) {
    let conn = pool.get().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL DEFAULT 'New Chat',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_id INTEGER NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );",
    )
    .unwrap();
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN images TEXT DEFAULT NULL", []);
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN tool_calls TEXT DEFAULT NULL",
        [],
    );
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN name TEXT DEFAULT NULL", []);
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending_tools (
            chat_id INTEGER PRIMARY KEY,
            tool_calls TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
    .unwrap();
}

pub fn list_chats(pool: &DbPool) -> Result<Vec<ChatSummary>, String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.title, c.created_at, COUNT(m.id) as message_count
             FROM chats c
             LEFT JOIN messages m ON m.chat_id = c.id
             GROUP BY c.id
             ORDER BY c.created_at DESC, c.id DESC",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ChatSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                message_count: row.get(3)?,
            })
        })
        .map_err(|e| format!("{e}"))?;
    let mut chats = Vec::new();
    for row in rows {
        chats.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(chats)
}

pub fn create_chat(pool: &DbPool) -> Result<ChatSummary, String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    conn.execute("INSERT INTO chats (title) VALUES ('New Chat')", [])
        .map_err(|e| format!("{e}"))?;
    let id = conn.last_insert_rowid();
    let mut stmt = conn
        .prepare("SELECT id, title, created_at FROM chats WHERE id = ?1")
        .map_err(|e| format!("{e}"))?;
    let chat = stmt
        .query_row(params![id], |row| {
            Ok(ChatSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                message_count: 0,
            })
        })
        .map_err(|e| format!("{e}"))?;
    Ok(chat)
}

pub fn chat_exists(pool: &DbPool, id: i64) -> Result<bool, String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chats WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|e| format!("{e}"))?;
    Ok(count > 0)
}

pub fn delete_chat(pool: &DbPool, id: i64) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    conn.execute("DELETE FROM pending_tools WHERE chat_id = ?1", params![id])
        .map_err(|e| format!("{e}"))?;
    conn.execute("DELETE FROM messages WHERE chat_id = ?1", params![id])
        .map_err(|e| format!("{e}"))?;
    conn.execute("DELETE FROM chats WHERE id = ?1", params![id])
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

pub fn get_messages(pool: &DbPool, chat_id: i64) -> Result<Vec<MessageOut>, String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, role, content, created_at, images, name, tool_calls
             FROM messages
             WHERE chat_id = ?1
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map(params![chat_id], |row| {
            let images_json: Option<String> = row.get(4)?;
            let images = images_json
                .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok());
            let name: Option<String> = row.get(5)?;
            let tool_calls_json: Option<String> = row.get(6)?;
            let tool_calls = tool_calls_json
                .and_then(|j| serde_json::from_str::<Vec<ToolCall>>(&j).ok())
                .filter(|t| !t.is_empty());
            Ok(MessageOut {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                created_at: row.get(3)?,
                images,
                name,
                tool_calls,
            })
        })
        .map_err(|e| format!("{e}"))?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(messages)
}

pub fn add_message(
    pool: &DbPool,
    chat_id: i64,
    role: &str,
    content: &str,
    images: Option<&[String]>,
) -> Result<(), String> {
    add_message_with_name(pool, chat_id, role, content, None, images)
}

pub fn add_message_with_name(
    pool: &DbPool,
    chat_id: i64,
    role: &str,
    content: &str,
    name: Option<&str>,
    images: Option<&[String]>,
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let images_json = images
        .map(|imgs| serde_json::to_string(imgs).map_err(|e| format!("JSON: {e}")))
        .transpose()?;
    conn.execute(
        "INSERT INTO messages (chat_id, role, content, name, images) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![chat_id, role, content, name, images_json],
    )
    .map_err(|e| format!("{e}"))?;
    Ok(())
}

pub fn update_title_from_message(
    pool: &DbPool,
    chat_id: i64,
    message: &str,
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let current_title: String = conn
        .query_row(
            "SELECT title FROM chats WHERE id = ?1",
            params![chat_id],
            |row| row.get(0),
        )
        .unwrap_or_default();
    if current_title == "New Chat" {
        let new_title: String = message.chars().take(50).collect();
        conn.execute(
            "UPDATE chats SET title = ?1 WHERE id = ?2",
            params![new_title, chat_id],
        )
        .map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

fn build_ollama_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("Failed to build Ollama HTTP client")
}

pub fn shared_ollama_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_ollama_http_client)
}

pub async fn query_ollama(
    ollama_url: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let client = shared_ollama_http_client();
    let request = OllamaRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
    };

    let response = client
        .post(format!("{}/api/generate", ollama_url))
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("Failed to connect to Ollama: {e}"))?;

    let mut full_response = String::new();
    let mut stream = Box::pin(response.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream error: {e}"))?;
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(data) = serde_json::from_str::<OllamaChunk>(line) {
                let cleaned = data
                    .response
                    .replace("<think>", "")
                    .replace("</think>", "");
                full_response.push_str(&cleaned);
            }
        }
    }
    Ok(full_response)
}

pub fn get_env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

// ── Ollama Chat API types (for tool-enabled chat) ──

fn deserialize_string_or_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaChatMessage {
    pub role: String,
    #[serde(deserialize_with = "deserialize_string_or_empty")]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct OllamaChatRequest {
    pub model: String,
    pub messages: Vec<OllamaChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<tools::ToolDefinition>>,
}

#[derive(Debug, Deserialize)]
pub struct OllamaChatResponse {
    pub message: OllamaChatMessage,
    pub done: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaShowResponse {
    capabilities: Option<Vec<String>>,
}

static VISION_CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

pub async fn model_supports_vision(ollama_url: &str, model: &str) -> bool {
    let cache = VISION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let map = cache.lock().unwrap();
        if let Some(&cached) = map.get(model) {
            return cached;
        }
    }
    let client = shared_ollama_http_client();
    let resp = client
        .post(format!("{}/api/show", ollama_url))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await;
    let supports = match resp {
        Ok(r) => {
            let bytes = r.bytes().await.unwrap_or_default();
            serde_json::from_slice::<OllamaShowResponse>(&bytes)
                .ok()
                .and_then(|s| s.capabilities)
                .map(|caps| caps.iter().any(|c| c == "vision"))
                .unwrap_or(false)
        }
        Err(_) => false,
    };
    let mut map = cache.lock().unwrap();
    map.insert(model.to_string(), supports);
    supports
}

pub fn get_coding_system_prompt() -> String {
    "You are pj, a pragmatic coding agent working directly in the user's filesystem. \
    You get real work done with tools instead of describing what could be done.\n\n\
    Working rules:\n\
    - Read before you edit: inspect relevant files with read_file/grep/glob so edits match reality.\n\
    - Prefer edit_file with a unique old_string for targeted changes; use write_file only for new files or full rewrites.\n\
    - After code changes, verify with run_command (build/test) when it makes sense.\n\
    - Chain multiple tool calls autonomously to finish the task; execution is already gated by the UI, so never ask the user for permission between steps.\n\
    - Keep prose short: report what you did, what changed, and anything the user must decide. No filler.\n\
    If you cannot use tools for something, say so plainly."
        .to_string()
}

pub fn build_messages_from_db(pool: &DbPool, chat_id: i64) -> Result<Vec<OllamaChatMessage>, String> {
    let db_msgs = get_messages(pool, chat_id)?;
    let mut messages = Vec::new();
    messages.push(OllamaChatMessage {
        role: "system".to_string(),
        content: get_coding_system_prompt(),
        tool_calls: None,
        name: None,
        images: None,
    });
    for m in &db_msgs {
        let tool_calls = m.tool_calls.clone().filter(|t| !t.is_empty());
        if m.role == "assistant" && tool_calls.is_none() && m.content.trim().is_empty() {
            continue;
        }
        messages.push(OllamaChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls,
            name: None,
            images: m.images.clone(),
        });
    }
    Ok(messages)
}

pub fn add_tool_result_message(
    pool: &DbPool,
    chat_id: i64,
    tool_name: &str,
    result: &str,
) -> Result<(), String> {
    add_message_with_name(pool, chat_id, "tool", result, Some(tool_name), None)
}

pub fn add_assistant_message_with_tools(
    pool: &DbPool,
    chat_id: i64,
    text: &str,
    tool_calls: &[ToolCall],
) -> Result<(), String> {
    let calls_json = serde_json::to_string(tool_calls).map_err(|e| format!("JSON: {e}"))?;
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    conn.execute(
        "INSERT INTO messages (chat_id, role, content, tool_calls) VALUES (?1, 'assistant', ?2, ?3)",
        params![chat_id, text, calls_json],
    )
    .map_err(|e| format!("{e}"))?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PendingToolCall {
    pub tool_calls: Vec<ToolCall>,
}

pub fn save_pending_tools(pool: &DbPool, chat_id: i64, calls: &[ToolCall]) -> Result<(), String> {
    let json = serde_json::to_string(&PendingToolCall {
        tool_calls: calls.to_vec(),
    })
    .map_err(|e| format!("JSON: {e}"))?;
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    conn.execute(
        "INSERT INTO pending_tools (chat_id, tool_calls) VALUES (?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET tool_calls = excluded.tool_calls",
        params![chat_id, json],
    )
    .map_err(|e| format!("{e}"))?;
    Ok(())
}

pub fn load_pending_tools(pool: &DbPool, chat_id: i64) -> Result<Option<Vec<ToolCall>>, String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    let json: Option<String> = conn
        .query_row(
            "SELECT tool_calls FROM pending_tools WHERE chat_id = ?1",
            params![chat_id],
            |row| row.get(0),
        )
        .ok();
    match json {
        Some(j) => {
            let parsed: PendingToolCall = serde_json::from_str(&j)
                .map_err(|e| format!("Corrupt pending tools for chat {chat_id}: {e}"))?;
            Ok(Some(parsed.tool_calls))
        }
        None => Ok(None),
    }
}

pub fn delete_pending_tools(pool: &DbPool, chat_id: i64) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("Pool: {e}"))?;
    conn.execute("DELETE FROM pending_tools WHERE chat_id = ?1", params![chat_id])
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

pub async fn chat_with_ollama(
    ollama_url: &str,
    model: &str,
    messages: Vec<OllamaChatMessage>,
    tools: Option<Vec<tools::ToolDefinition>>,
) -> Result<OllamaChatResponse, String> {
    let client = shared_ollama_http_client();

    let url = format!("{}/api/chat", ollama_url);
    let mut active_tools = tools;
    let working_messages = messages;

    loop {
        let request = OllamaChatRequest {
            model: model.to_string(),
            messages: working_messages.clone(),
            stream: false,
            tools: active_tools.clone(),
        };
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to connect to Ollama: {e}"))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read Ollama response body: {e}"))?;

        let text = String::from_utf8_lossy(&bytes).to_string();

        if !status.is_success() {
            let err = format!("Ollama returned error {status}: {text}");
            if active_tools.is_some() && (err.contains("does not support tools") || err.contains("not supported")) {
                log::warn!("Model does not support tools, retrying without: {err}");
                active_tools = None;
                continue;
            }
            return Err(err);
        }

        let resp: OllamaChatResponse = serde_json::from_str(&text).map_err(|e| {
            let preview: String = text.chars().take(500).collect();
            format!("Failed to parse Ollama response ({e}): {preview}")
        })?;
        return Ok(resp);
    }
}

pub enum TurnOutcome {
    Reply(String),
    PendingTools(Vec<ToolCall>),
}

pub async fn run_chat_turn(
    pool: &DbPool,
    ollama_url: &str,
    model: &str,
    chat_id: i64,
) -> Result<TurnOutcome, String> {
    let msgs = build_messages_from_db(pool, chat_id)?;
    let resp = chat_with_ollama(ollama_url, model, msgs, Some(tools::get_tool_definitions())).await?;

    let native_tcs = resp.message.tool_calls.clone().unwrap_or_default();
    let clean_text = tools::strip_tool_calls_from_text(&resp.message.content);
    let tool_calls = tools::normalize_tool_calls(&(if !native_tcs.is_empty() {
        native_tcs
    } else {
        tools::parse_tool_calls_from_text(&resp.message.content)
    }));

    if tool_calls.is_empty() && clean_text.trim().is_empty() {
        return Ok(TurnOutcome::Reply(String::new()));
    }

    if tool_calls.is_empty() {
        add_message(pool, chat_id, "assistant", &clean_text, None)?;
        publish_chat_change(&ChatChange::Upsert { id: chat_id }).ok();
        return Ok(TurnOutcome::Reply(clean_text));
    }

    add_assistant_message_with_tools(pool, chat_id, &clean_text, &tool_calls)?;
    save_pending_tools(pool, chat_id, &tool_calls)?;
    publish_chat_change(&ChatChange::Upsert { id: chat_id }).ok();
    publish_chat_change(&ChatChange::Activity {
        id: chat_id,
        state: ChatActivityState::AwaitingToolConfirmation,
    })
    .ok();
    Ok(TurnOutcome::PendingTools(tool_calls))
}

pub async fn resolve_pending_tools(
    pool: &DbPool,
    ollama_url: &str,
    model: &str,
    chat_id: i64,
    approved: bool,
) -> Result<TurnOutcome, String> {
    let calls = load_pending_tools(pool, chat_id)?.ok_or_else(|| {
        format!("No pending tool calls for chat {chat_id} (already resolved elsewhere?)")
    })?;
    delete_pending_tools(pool, chat_id)?;

    for tc in &calls {
        let result = if approved {
            match tools::execute_tool(tc) {
                Ok(r) => r,
                Err(e) => format!("Error: {e}"),
            }
        } else {
            "User declined to execute this tool call.".to_string()
        };
        add_tool_result_message(pool, chat_id, &tc.function.name, &result)?;
    }

    publish_chat_change(&ChatChange::Upsert { id: chat_id }).ok();
    publish_chat_change(&ChatChange::Activity {
        id: chat_id,
        state: ChatActivityState::Thinking,
    })
    .ok();

    run_chat_turn(pool, ollama_url, model, chat_id).await
}
