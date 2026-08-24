# pj — Coding Agent Guide

## Project Overview

pj is a local-first Rust coding agent. It provides a web UI (Actix-web) and a CLI with both a Ratatui fullscreen mode and a plain terminal mode, all backed by Ollama (local or Ollama cloud models) for inference and SQLite for chat persistence. Both frontends drive the same shared agent engine and the same database, so a chat can be started in one surface and continued seamlessly in the other.

## Build & Run

```bash
# Build both binaries
cargo build --release

# Web UI (default binary)
cargo run --release --bin pj-web

# CLI default interactive mode
cargo run --release --bin pj

# CLI plain terminal mode
cargo run --release --bin pj -- --plain

# CLI force fullscreen TUI mode
cargo run --release --bin pj -- --tui

# CLI one-shot
cargo run --release --bin pj -- "your question here"

# Lint / typecheck (Rust — cargo check covers this)
cargo check
```

There are no unit tests. `cargo check` is the minimum verification step.

Agent workflow note: after every code change, run `cargo check`, `cargo clippy`, and `cargo build --release` so linting, compilation, and user-testable binaries are all covered before handoff.

## Architecture

```
src/
  main.rs   — pj-web binary: Actix-web server, REST API, serves static UI
  cli.rs    — pj binary: Ratatui TUI, plain terminal mode, one-shot CLI mode
  lib.rs    — Shared: DB (SQLite via r2d2), Ollama API client, prompts, tool enforcement, types
  tools.rs  — Tool definitions, execution, text-based tool call parsing
static/     — Web UI frontend (HTML/JS/CSS)
  fonts/    — Bundled web fonts including local CJK coverage
data/       — SQLite database (chat.db)
```

### Two binaries, one lib

- **pj-web** (`main.rs`): HTTP server on configurable PORT. Endpoints:
  - `POST /api/chat` — streaming generate (basic, no tools)
  - `POST /api/chat/tools` — chat with tool support
  - `POST /api/chat/tools/confirm` — approve/deny pending tools for a chat (`{chat_id, approved}`)
  - `GET /api/chats/{id}/pending-tools` — fetch a chat's pending tool calls (used to show approval prompts initiated from another surface)
  - `GET /api/events` — server-sent events for chat change sync
  - `POST /api/write-file` — direct web save helper for code blocks
  - CRUD for chats/messages
- **pj** (`cli.rs`): default interactive mode, plain terminal mode (`--plain`), fullscreen TUI (`--tui`), or one-shot (pass question as args). Shares the same SQLite DB as the web UI.

### Shared agent engine

All tool flows (web and CLI) go through the same engine functions in `lib.rs`:

- `run_chat_turn(pool, url, model, chat_id)` — rebuilds context from the DB, calls the model, persists the assistant reply or tool call, and stores unapproved tool calls in the `pending_tools` table. Returns `TurnOutcome::Reply(text)` or `TurnOutcome::PendingTools(calls)`.
- `resolve_pending_tools(..., approved)` — executes (or declines) a chat's pending tools, persists each result as a `role='tool'` message, deletes the pending row, and immediately runs the next turn.

Because pending approvals live in the database (not memory), either surface can approve work requested by the other. Chat change events (`ChatChange::Upsert/Activity`) keep both surfaces live.

### Ollama Integration

- Base URL: `OLLAMA_URL` env var (default `http://localhost:11434`)
- Model selection: `resolve_model()` in `lib.rs` checks `MODEL_NAME` first, then `MODEL_PRESET`. Presets: `speed` → `gpt-oss:20b-cloud`, `balanced` (default) → `gpt-oss:120b-cloud`, `quality` → `minimax-m3:cloud`, plus `local-*` presets (`local-speed|local-balanced|local-quality`) for offline qwen2.5 models.
- Cloud models use the same local Ollama endpoint (Ollama proxies to ollama.com; requires `ollama signin`). The free tier includes gpt-oss:20b/120b-cloud and minimax-m3:cloud; glm/deepseek/qwen3.5/kimi cloud variants require a subscription.
- Web UI basic chat uses `/api/generate` (simple, streaming, no tools)
- Web UI tool chat and CLI use `/api/chat` (non-streaming, with tools)
- `chat_with_ollama()` in `lib.rs` is the shared chat API client; if the model doesn't support tools it auto-retries without them

### Tool System

Tools are defined in `tools.rs`. Available tools: `read_file`, `write_file`, `edit_file`, `run_command`, `glob`, `grep`, `read_directory`.

Tool behavior (enforced in Rust, not just prompted):
- Relative paths are resolved against the process working directory, so models can edit project files without absolute paths.
- `edit_file` requires a unique `old_string` match (or `replace_all: true`) and errors with guidance on ambiguity.
- `read_file` returns numbered lines and supports `offset`/`limit`.
- `run_command` has a default 120s timeout (`timeout_secs` up to 600), captures stdout+stderr plus exit code, and truncates oversized output.
- All tool outputs are capped (~30KB) to protect the context window.

Two tool detection paths:
1. **Native**: Ollama returns `tool_calls` in the response JSON (for models that support it)
2. **Text fallback**: `parse_tool_calls_from_text()` extracts `<tool_call>` XML tags from response text

Tool execution requires user confirmation in both CLI (y/N prompt) and web UI (confirm endpoint).

Behavioral guarantees should be enforced in Rust wherever practical, not only described in the system prompt. When changing tool behavior, prefer shared checks in `lib.rs`/`tools.rs` over adding more prompt text.

Similarly, no image/vision limitations should be hardcoded in the system prompt — the user may swap in a vision-capable model at any time. Capability checks (e.g. whether the model supports images) belong in code, not in prompt text that would artificially cap a model's abilities.

### Database

SQLite via r2d2 connection pool. Schema:
- `chats` — id, title, created_at
- `messages` — id, chat_id, role, content, created_at, images, tool_calls (JSON for assistant messages that request tools), name (tool name on `role='tool'` rows)
- `pending_tools` — chat_id (PK), tool_calls JSON; one unapproved batch per chat

DB path: `DATABASE_URL` env var. If unset, both binaries default to the project database at `/home/whiterabbit/CodingStuff/area51/aiMagic/personalJesus/data/chat.db`.

## Code Conventions

- No comments in code unless explicitly asked
- Error handling: propagate with `map_err` and `format!`, never `unwrap()` in production paths (DB pool init is the exception)
- Async: tokio runtime, `tokio::spawn` for background work
- Serialization: serde + serde_json, `#[serde(skip_serializing_if)]` for optional fields
- Environment config: `get_env_or(key, default)` pattern throughout
- Frontend: vanilla JS in `static/`, no build step
- Documentation hygiene: when code changes behavior, verify `AGENTS.md` and `README.md` still match the current code and update them in the same change when needed

## Known Quirks

- Models that don't support tools (like gemma2) will error on `/api/chat` with tools — the auto-retry handles this
- `OllamaChatMessage.content` uses a custom deserializer to handle `null` from Ollama (models return null content when issuing tool calls)
- The TUI uses crossterm raw mode + alternate screen; panics in the TUI loop will leave the terminal in a broken state (run `reset` to fix)
- `--plain` is the manual fallback when the fullscreen TUI is a poor fit for terminal font/rendering behavior
- Web UI Chinese rendering depends on the bundled local `Noto Sans CJK SC` font in `static/fonts/`; CLI Chinese rendering still depends on terminal font support and may require a terminal restart after font installation
- The tool-enabled flows still support text `<tool_call>` fallback for models without native tool support, so prompt and parser changes can affect both CLI and web
- Both binaries share `lib.rs` — changes to Ollama types affect both
