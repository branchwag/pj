use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

const MAX_TOOL_OUTPUT_BYTES: usize = 30_000;

fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output.to_string();
    }
    let head: String = output.chars().take(MAX_TOOL_OUTPUT_BYTES / 2).collect();
    let tail_start = output.chars().count().saturating_sub(MAX_TOOL_OUTPUT_BYTES / 2);
    let tail: String = output.chars().skip(tail_start).collect();
    format!(
        "{head}\n\n[... output truncated, {truncated} characters omitted ...]\n\n{tail}",
        truncated = output.len() - head.len() - tail.len()
    )
}

fn absolute_path_in(path: &str, base: Option<&Path>) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_string_lossy().to_string()
    } else {
        match base.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok()) {
            Some(cwd) => cwd.join(p).to_string_lossy().to_string(),
            None => p.to_string_lossy().to_string(),
        }
    }
}

fn cwd_display() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

pub fn normalize_tool_call_in(tool_call: &ToolCall, base: Option<&Path>) -> ToolCall {
    let mut normalized = tool_call.clone();
    let name = normalized.function.name.as_str();
    if !matches!(name, "write_file" | "edit_file" | "read_file" | "read_directory") {
        return normalized;
    }

    if let Some(args) = normalized.function.arguments.as_object_mut()
        && let Some(path) = args.get("path").and_then(|v| v.as_str())
    {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            args.insert(
                "path".to_string(),
                serde_json::Value::String(absolute_path_in(trimmed, base)),
            );
        }
    }

    normalized
}

pub fn normalize_tool_call(tool_call: &ToolCall) -> ToolCall {
    normalize_tool_call_in(tool_call, None)
}

pub fn normalize_tool_calls_in(tool_calls: &[ToolCall], base: Option<&Path>) -> Vec<ToolCall> {
    tool_calls.iter().map(|tc| normalize_tool_call_in(tc, base)).collect()
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "read_file".to_string(),
                description: "Read a file with numbered lines. For large files, use offset/limit to read sections. Always read before editing.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file (absolute, or relative to the working directory)" },
                        "offset": { "type": "integer", "description": "1-based line number to start reading from (default: 1)" },
                        "limit": { "type": "integer", "description": "Maximum number of lines to read (default: 2000)" }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "write_file".to_string(),
                description: "Write content to a file, overwriting it and creating parent directories. Use for new files or complete rewrites only; prefer edit_file for targeted changes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file (absolute, or relative to the working directory)" },
                        "content": { "type": "string", "description": "Content to write" }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "edit_file".to_string(),
                description: "Replace an exact unique text match in a file. old_string must appear exactly once unless replace_all is true; include surrounding lines to make it unique.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file (absolute, or relative to the working directory)" },
                        "old_string": { "type": "string", "description": "Text to search for (exact match, must be unique in the file)" },
                        "new_string": { "type": "string", "description": "Replacement text" },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of failing on multiple matches (default: false)" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "run_command".to_string(),
                description: "Run a shell command via `sh -c` and capture stdout/stderr plus the exit code. Use for builds, tests, git, and other verification. Times out if a command hangs.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 600)" }
                    },
                    "required": ["command"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "glob".to_string(),
                description: "Find files matching a glob pattern, sorted by modification time (newest first). Pattern like '**/*.rs'.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern (e.g. 'src/**/*.rs')" },
                        "path": { "type": "string", "description": "Base directory to search from (default: working directory)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "grep".to_string(),
                description: "Search file contents with a regex. Uses ripgrep if available, falls back to grep. Returns file:line:match rows.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern to search" },
                        "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                        "include": { "type": "string", "description": "Glob filter for files to search (e.g. '*.rs')" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "read_directory".to_string(),
                description: "List a directory's entries; directories get a trailing slash.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the directory (absolute, or relative to the working directory)" }
                    },
                    "required": ["path"]
                }),
            },
        },
    ]
}

pub fn execute_tool(tool_call: &ToolCall) -> Result<String, String> {
    execute_tool_with_base(tool_call, None)
}

pub fn execute_tool_with_base(tool_call: &ToolCall, base: Option<&Path>) -> Result<String, String> {
    let name = &tool_call.function.name;
    let args = &tool_call.function.arguments;

    match name.as_str() {
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("read_file error: {e}"))?;
            let total_lines = content.lines().count();
            let offset = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v.max(1) as usize - 1)
                .unwrap_or(0);
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v.max(1) as usize)
                .unwrap_or(2000);
            let selected: Vec<&str> = content
                .lines()
                .skip(offset)
                .take(limit)
                .collect();
            let mut out = String::new();
            for (i, line) in selected.iter().enumerate() {
                out.push_str(&format!("{:>6}\t{}\n", offset + i + 1, line));
            }
            if out.len() > MAX_TOOL_OUTPUT_BYTES {
                let cut: String = out.chars().take(MAX_TOOL_OUTPUT_BYTES).collect();
                out = format!("{cut}\n[... file truncated ...]");
            }
            let range_note = format!(
                "\n[{showing} of {total} total lines]",
                showing = selected.len(),
                total = total_lines
            );
            out.push_str(&range_note);
            Ok(out)
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
            let content = args.get("content").and_then(|v| v.as_str()).ok_or("missing content")?;
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("write_file mkdir error: {e}"))?;
            }
            std::fs::write(path, content).map_err(|e| format!("write_file error: {e}"))?;
            Ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path
            ))
        }
        "edit_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
            let old = args.get("old_string").and_then(|v| v.as_str()).ok_or("missing old_string")?;
            let new = args.get("new_string").and_then(|v| v.as_str()).ok_or("missing new_string")?;
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if old == new {
                return Err("old_string and new_string are identical; nothing to change".to_string());
            }
            let content =
                std::fs::read_to_string(path).map_err(|e| format!("edit_file read error: {e}"))?;
            let occurrences = content.matches(old).count();
            if occurrences == 0 {
                return Err(format!(
                    "old_string not found in {path}. Re-read the file and include enough surrounding lines to match exactly."
                ));
            }
            if occurrences > 1 && !replace_all {
                return Err(format!(
                    "old_string matches {occurrences} locations in {path}; it must be unique. Add surrounding lines to disambiguate or set replace_all=true."
                ));
            }
            let new_content = if replace_all {
                content.replace(old, new)
            } else {
                content.replacen(old, new, 1)
            };
            std::fs::write(path, new_content)
                .map_err(|e| format!("edit_file write error: {e}"))?;
            Ok(format!(
                "edited {path}: replaced {replaced} occurrence(s)",
                replaced = if replace_all { occurrences } else { 1 }
            ))
        }
        "run_command" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).ok_or("missing command")?;
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .map(|v| v.clamp(1, 600))
                .unwrap_or(120);
            let start = std::time::Instant::now();
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(cmd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(dir) = base {
                command.current_dir(dir);
            }
            let mut child = command
                .spawn()
                .map_err(|e| format!("run_command spawn error: {e}"))?;

            fn drain<T: std::io::Read>(pipe: Option<T>) -> String {
                let mut buf = String::new();
                if let Some(mut p) = pipe {
                    let _ = std::io::Read::read_to_string(&mut p, &mut buf);
                }
                buf
            }

            let stdout_pipe = child.stdout.take();
            let stderr_pipe = child.stderr.take();
            let out_reader = std::thread::spawn(move || drain(stdout_pipe));
            let err_reader = std::thread::spawn(move || drain(stderr_pipe));

            let deadline = start + std::time::Duration::from_secs(timeout_secs);
            let status = loop {
                match child.try_wait().map_err(|e| format!("run_command wait error: {e}"))? {
                    Some(status) => break status,
                    None => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            let partial_out = out_reader.join().unwrap_or_default();
                            let partial_err = err_reader.join().unwrap_or_default();
                            let mut partial = String::new();
                            if !partial_out.trim().is_empty() {
                                partial.push_str(partial_out.trim_end());
                            }
                            if !partial_err.trim().is_empty() {
                                if !partial.is_empty() {
                                    partial.push('\n');
                                }
                                partial.push_str(partial_err.trim_end());
                            }
                            let note = if partial.trim().is_empty() {
                                "no output was produced".to_string()
                            } else {
                                format!("output so far:\n{}", truncate_output(partial.trim()))
                            };
                            return Err(format!(
                                "command timed out after {timeout_secs}s and was killed; {note}"
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            };

            let stdout = out_reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            let mut result = String::new();
            if !stdout.trim().is_empty() {
                result.push_str(stdout.trim_end());
            }
            if !stderr.trim().is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(stderr.trim_end());
            }
            result.push_str(&format!(
                "\nexit code: {} ({:.1}s)",
                status.code().unwrap_or(-1),
                start.elapsed().as_secs_f32()
            ));
            Ok(truncate_output(&result))
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("missing pattern")?;
            let root = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| absolute_path_in(p, base))
                .unwrap_or_else(|| {
                    base.map(Path::to_string_lossy)
                        .map(|p| p.to_string())
                        .unwrap_or_else(cwd_display)
                });
            let full_pattern = if Path::new(pattern).is_absolute() {
                pattern.to_string()
            } else {
                format!("{}/{}", root.trim_end_matches('/'), pattern)
            };
            let entries = glob::glob(&full_pattern).map_err(|e| format!("glob error: {e}"))?;
            let mut paths: Vec<(std::time::SystemTime, String)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|p| {
                    let meta = p.metadata().ok()?;
                    let mtime = meta.modified().ok()?;
                    Some((mtime, p.display().to_string()))
                })
                .collect();
            paths.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
            if paths.is_empty() {
                return Ok("no matches".to_string());
            }
            let truncated = paths.len() > 1000;
            let list: Vec<String> = paths
                .into_iter()
                .take(1000)
                .map(|(_, p)| p)
                .collect();
            let mut out = list.join("\n");
            if truncated {
                out.push_str("\n[... more than 1000 matches, showing newest 1000 ...]");
            }
            Ok(truncate_output(&out))
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("missing pattern")?;
            let search_path = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| absolute_path_in(p, base))
                .unwrap_or_else(|| {
                    base.map(Path::to_string_lossy)
                        .map(|p| p.to_string())
                        .unwrap_or_else(cwd_display)
                });
            let include = args.get("include").and_then(|v| v.as_str());

            let mut rg_result = std::process::Command::new("rg");
            rg_result.args(["-n", "--no-heading"]).arg(pattern).arg(&search_path);
            match include {
                Some(glob_filter) => rg_result.args(["-g", glob_filter]),
                None => &mut rg_result,
            };
            let rg_result = rg_result.output();

            let output = match rg_result {
                Ok(o) if o.status.success() || o.status.code() == Some(1) => o,
                _ => {
                    let grep_cmd = match include {
                        Some(glob_filter) => vec!["-rn".to_string(), format!("--include={glob_filter}")],
                        None => vec!["-rn".to_string()],
                    };
                    let mut c = std::process::Command::new("grep");
                    c.args(&grep_cmd);
                    c.arg(pattern).arg(&search_path);
                    c.output().map_err(|e| format!("grep error: {e}"))?
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if stdout.trim().is_empty() {
                return Ok("no matches".to_string());
            }
            Ok(truncate_output(stdout.trim()))
        }
        "read_directory" => {
            let dir = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
            let dir_abs = absolute_path_in(dir, base);
            let entries = std::fs::read_dir(&dir_abs).map_err(|e| format!("read_dir error: {e}"))?;
            let mut items: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect();
            items.sort();
            if items.is_empty() {
                Ok("empty directory".to_string())
            } else {
                Ok(truncate_output(&items.join("\n")))
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

pub fn parse_tool_calls_from_text(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut pos = 0;
    let bytes = text.as_bytes();
    while pos < bytes.len() {
        let remaining = &text[pos..];
        if let Some(start) = remaining.find("<tool_call>") {
            let content_start = pos + start + "<tool_call>".len();
            if let Some(end) = text[content_start..].find("</tool_call>") {
                let json_str = &text[content_start..content_start + end];
                if let Ok(tc) = serde_json::from_str::<ToolCall>(json_str.trim()) {
                    calls.push(tc);
                }
                pos = content_start + end + "</tool_call>".len();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    calls
}

pub fn strip_tool_calls_from_text(text: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    loop {
        let remaining = &text[pos..];
        if let Some(start) = remaining.find("<tool_call>") {
            result.push_str(&text[pos..pos + start]);
            if let Some(end) = remaining[start..].find("</tool_call>") {
                pos = pos + start + end + "</tool_call>".len();
            } else {
                result.push_str(remaining);
                break;
            }
        } else {
            result.push_str(remaining);
            break;
        }
    }
    let stripped = result.trim().to_string();
    strip_think_tags(&stripped)
}

fn strip_think_tags(text: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    loop {
        let remaining = &text[pos..];
        if let Some(start) = remaining.find("<think>") {
            result.push_str(&text[pos..pos + start]);
            if let Some(end) = remaining[start..].find("</think>") {
                pos = pos + start + end + "</think>".len();
            } else {
                break;
            }
        } else {
            result.push_str(remaining);
            break;
        }
    }
    result.trim().to_string()
}
#[derive(Debug, Clone)]
pub struct CodeBlock {
    pub language: String,
    pub code: String,
}

pub fn extract_code_blocks(text: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut pos = 0;
    let bytes = text.as_bytes();
    while pos < bytes.len() {
        let remaining = &text[pos..];
        if let Some(start) = remaining.find("```") {
            let content_start = pos + start + 3;
            let rest = &text[content_start..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let language = rest[..line_end].trim().to_string();
            let code_start = content_start + line_end + 1;
            if let Some(end) = text[code_start..].find("```") {
                let code = &text[code_start..code_start + end];
                blocks.push(CodeBlock { language, code: code.to_string() });
                pos = code_start + end + 3;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    blocks
}

pub fn tool_call_description(tool_call: &ToolCall) -> String {
    let name = &tool_call.function.name;
    let args = &tool_call.function.arguments;

    let g = |key: &str| -> String {
        args.get(key).and_then(|v| v.as_str()).unwrap_or("?").to_string()
    };

    match name.as_str() {
        "read_file" => format!("Read file: {}", g("path")),
        "write_file" => {
            let content = g("content");
            let preview: String = content.chars().take(80).collect();
            let ellipsis = if content.len() > 80 { "..." } else { "" };
            format!("Write file: {} ({} bytes)\n  {}{}", g("path"), content.len(), preview, ellipsis)
        }
        "edit_file" => {
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mode = if replace_all { " (all)" } else { "" };
            format!(
                "Edit file{}: {}\n  Replace: {}\n  With:    {}",
                mode,
                g("path"),
                g("old_string"),
                g("new_string")
            )
        }
        "run_command" => format!("Run: {}", g("command")),
        "glob" => format!("Glob: {}", g("pattern")),
        "grep" => {
            let p = g("path");
            let sp = if p == "?" { ".".to_string() } else { p };
            format!("Grep: '{}' in {}", g("pattern"), sp)
        }
        "read_directory" => format!("List dir: {}", g("path")),
        _ => format!("Tool: {name}({args})"),
    }
}
