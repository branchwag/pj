# pj

🎵 *Lift up the receiver, I'll make you a believer* 🎵

A local-first Rust coding agent. The web UI and the CLI (fullscreen TUI or plain terminal) drive the same shared agent engine and the same SQLite database, so you can start a task in the browser, approve its tool calls from the terminal, and pick any chat back up in either surface. Inference runs through Ollama — fully offline with a local model, or via Ollama's cloud tier for bigger coding models. All static assets are bundled locally.

![Demo Screenshot](./pjdemo.png)

## Quick Start

1. **Prerequisites**:
   - [Rust](https://rustup.rs/) (1.75 or newer)
   - [Ollama](https://ollama.ai/) installed locally

2. **Pull a model** (or use the default via presets):
   ```bash
   ollama pull gpt-oss:120b-cloud
   ```

3. **Set environment variables** (all optional):
   ```bash
   export OLLAMA_URL=http://localhost:11434
   export MODEL_PRESET=balanced   # speed | balanced (default) | quality | local-*
   export PORT=8080
   ```
   Or override the model directly:
   ```bash
   export MODEL_NAME=gemma2:9b
   ```

4. **Run the application**:
   ```bash
   cargo run --release
   ```

5. **Access the app**:
   Open your browser to [http://localhost:8080](http://localhost:8080)

## Model Presets

pj supports model presets for easy speed/quality tuning. Set `MODEL_PRESET` to choose one:

| Preset           | Model              | Notes                                              |
|------------------|--------------------|----------------------------------------------------|
| `speed`          | gpt-oss:20b-cloud  | Fast free cloud model with solid tool calling      |
| `balanced`       | gpt-oss:120b-cloud | Strong open coding model, Ollama cloud free tier (default) |
| `quality`        | minimax-m3:cloud   | Agentic cloud model with 1M token context          |
| `local-speed`    | qwen2.5:1.5b       | Local, fully offline, fastest                      |
| `local-balanced` | qwen2.5:3b         | Local, offline, balanced                           |
| `local-quality`  | qwen2.5:7b         | Local, offline, best quality                       |

Cloud models run through your local Ollama (it proxies to ollama.com; run `ollama signin` once). The free tier covers the three cloud presets above.

`MODEL_NAME` always takes precedence over `MODEL_PRESET` if both are set.

## CLI Tool

A command-line interface is available for chatting from the terminal:

```bash
# One-shot: ask a question and get a response
./target/release/pj "What is the capital of France?"

# Default interactive mode
./target/release/pj

# Plain terminal mode
./target/release/pj --plain

# Force fullscreen TUI mode
./target/release/pj --tui
```

To use `pj` from anywhere, add this alias to your `~/.bashrc` (make sure `DATABASE_URL` points to the project's database so the CLI and web app share the same data):

```bash
alias pj='DATABASE_URL=/path/to/pj/data/chat.db /path/to/pj/target/release/pj'
```

The CLI shares the same SQLite database as the web app, so conversations are synced between both interfaces. If `DATABASE_URL` is not set, both binaries now default to the project database at `/home/whiterabbit/CodingStuff/area51/aiMagic/personalJesus/data/chat.db` instead of using a cwd-relative path.

## Unicode and CJK Text

Chat content is stored in SQLite as UTF-8. If Chinese text looks wrong, the failure is usually in the rendering surface:

- Web UI: the bundled `Special Elite` font only covers Latin text, so Chinese glyphs must come from an installed fallback font such as `Noto Sans CJK SC`, `PingFang SC`, or `Microsoft YaHei`.
- Web UI: the app now bundles a local `Noto Sans CJK SC` font for Chinese text, so browser rendering does not depend on host font packages.
- CLI/TUI: your terminal emulator must use a font with CJK glyph coverage and a UTF-8 locale such as `en_US.UTF-8`.
- Use `pj --plain` if the fullscreen TUI is a poor fit for your terminal setup or font rendering.
- If the CLI still shows boxes for Chinese text, install a CJK font for your terminal environment and fully restart the terminal emulator so it reloads font fallback data. On this machine, installing `Noto Sans CJK SC` into `~/.local/share/fonts` and restarting `kitty` was required.

You can verify the stored content directly with:

```bash
sqlite3 data/chat.db "select role, content from messages order by id desc limit 5;"
```

## Contributing

Feel free to open issues or submit pull requests!

## License

MIT

## Credits

- Built with [Actix-web](https://actix.rs/)
- Powered by [Ollama](https://ollama.ai/)
