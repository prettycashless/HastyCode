//! Tool implementations. Every tool is pure local I/O except `web_fetch`.
//!
//! Permission decisions happen in the agent before a tool runs; these functions
//! assume the call was authorized (the catastrophic deny-list is still enforced
//! by the gate, never here).

use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::permissions::Action;
use crate::providers::ToolSpec;

pub const MAX_READ_CHARS: usize = 200_000;
pub const MAX_OUTPUT_CHARS: usize = 30_000;

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
    /// Extra text shown to the user as a diff, when the tool produced one.
    pub diff: Option<String>,
}

impl ToolOutcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            diff: None,
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: output.into(),
            diff: None,
        }
    }

    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }
}

pub struct ToolContext {
    pub root: PathBuf,
}

impl ToolContext {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Tool definitions handed to the model.
pub fn specs() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, parameters: Value| ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    };
    vec![
        spec(
            "read_file",
            "Read a UTF-8 text file. Returns content with line numbers. Use offset/limit for large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the project or absolute"},
                    "offset": {"type": "integer", "description": "First line to read (1-based)"},
                    "limit": {"type": "integer", "description": "Maximum number of lines"}
                },
                "required": ["path"]
            }),
        ),
        spec(
            "write_file",
            "Create or overwrite a file with full content. Prefer edit_file for existing files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ),
        spec(
            "edit_file",
            "Replace an exact string in a file. Fails when the old string is missing or ambiguous.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        spec(
            "move_file",
            "Move or rename a file or directory.",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"}
                },
                "required": ["from", "to"]
            }),
        ),
        spec(
            "delete_file",
            "Delete a file, or an empty directory.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        ),
        spec(
            "make_dir",
            "Create a directory and any missing parents.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        ),
        spec(
            "list_dir",
            "List directory entries. Directories end with a slash.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Defaults to the project root"}},
                "required": []
            }),
        ),
        spec(
            "glob",
            "Find files by glob pattern (supports *, **, ?). Returns paths.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "e.g. src/**/*.ts"},
                    "path": {"type": "string", "description": "Directory to search from"}
                },
                "required": ["pattern"]
            }),
        ),
        spec(
            "grep",
            "Search file contents with a regular expression. Returns path:line: text.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex syntax"},
                    "path": {"type": "string"},
                    "glob": {"type": "string", "description": "Only files matching this glob"},
                    "ignore_case": {"type": "boolean", "description": "Case-insensitive match"},
                    "context": {"type": "integer", "description": "Lines of context around each match (0-10)"}
                },
                "required": ["pattern"]
            }),
        ),
        spec(
            "run_command",
            "Run a shell command in the project and return stdout, stderr and exit status.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_secs": {"type": "integer", "description": "Default 120"}
                },
                "required": ["command"]
            }),
        ),
        spec(
            "install_package",
            "Install a dependency using the project's package manager.",
            json!({
                "type": "object",
                "properties": {
                    "package": {"type": "string"},
                    "manager": {"type": "string", "description": "npm, pnpm, yarn, bun, cargo, pip, uv, go"}
                },
                "required": ["package"]
            }),
        ),
        spec(
            "open_app",
            "Open a file, folder or URL in the system's default application.",
            json!({
                "type": "object",
                "properties": {"target": {"type": "string"}},
                "required": ["target"]
            }),
        ),
        spec(
            "git",
            "Run a git command in the project (status, diff, add, commit, branch, log, ...).",
            json!({
                "type": "object",
                "properties": {"args": {"type": "string", "description": "Arguments after `git`"}},
                "required": ["args"]
            }),
        ),
        spec(
            "web_fetch",
            "Fetch a URL and return readable text (HTML tags stripped).",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        ),
        spec(
            "use_skill",
            "Load a skill's instructions into context. The skill list is in the system prompt.",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        ),
        spec(
            "spawn_subagent",
            "Run a focused task in a separate agent with its own model, in parallel. Returns its report.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short label, e.g. tests or review"},
                    "task": {"type": "string", "description": "Complete, self-contained instructions"},
                    "model": {"type": "string", "description": "Optional configured subagent key/model label"}
                },
                "required": ["name", "task"]
            }),
        ),
        spec(
            "finish",
            "Finish the task. Call this only after verifying the result.",
            json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}},
                "required": ["summary"]
            }),
        ),
    ]
}

/// Actions the permission gate must inspect before this tool may run.
pub fn actions_for(name: &str, args: &Value, ctx: &ToolContext) -> Vec<Action> {
    let path = |key: &str| args[key].as_str().map(|p| resolve(&ctx.root, p));
    match name {
        "read_file" => path("path")
            .map(|p| Action::ReadFile { path: p })
            .into_iter()
            .collect(),
        "write_file" => path("path")
            .map(|p| Action::WriteFile { path: p })
            .into_iter()
            .collect(),
        "edit_file" => path("path")
            .map(|p| Action::EditFile { path: p })
            .into_iter()
            .collect(),
        "move_file" => {
            let from = path("from");
            let to = path("to");
            match (from, to) {
                (Some(from), Some(to)) => vec![Action::MoveFile { from, to }],
                _ => Vec::new(),
            }
        }
        "delete_file" => path("path")
            .map(|p| Action::DeleteFile { path: p })
            .into_iter()
            .collect(),
        "make_dir" => path("path")
            .map(|p| Action::MakeDir { path: p })
            .into_iter()
            .collect(),
        "run_command" => vec![Action::RunCommand {
            command: args["command"].as_str().unwrap_or_default().to_string(),
        }],
        "install_package" => vec![Action::Install {
            what: args["package"].as_str().unwrap_or_default().to_string(),
        }],
        "open_app" => vec![Action::OpenApp {
            app: args["target"].as_str().unwrap_or_default().to_string(),
        }],
        "git" => vec![Action::Git {
            args: args["args"].as_str().unwrap_or_default().to_string(),
        }],
        "web_fetch" => vec![Action::Network {
            url: args["url"].as_str().unwrap_or_default().to_string(),
        }],
        "spawn_subagent" => vec![Action::SpawnSubagent {
            name: args["name"].as_str().unwrap_or_default().to_string(),
        }],
        _ => Vec::new(),
    }
}

/// One-line description for the transcript.
pub fn summarize(name: &str, args: &Value) -> String {
    let s = |key: &str| args[key].as_str().unwrap_or_default();
    match name {
        "read_file" => format!("read {}", s("path")),
        "write_file" => format!("write {} ({} bytes)", s("path"), s("content").len()),
        "edit_file" => format!("edit {}", s("path")),
        "move_file" => format!("move {} -> {}", s("from"), s("to")),
        "delete_file" => format!("delete {}", s("path")),
        "make_dir" => format!("mkdir {}", s("path")),
        "list_dir" => format!(
            "list {}",
            if s("path").is_empty() { "." } else { s("path") }
        ),
        "glob" => format!("glob {}", s("pattern")),
        "grep" => format!("grep /{}/", s("pattern")),
        "run_command" => format!("$ {}", one_line(s("command"), 120)),
        "install_package" => format!("install {}", s("package")),
        "open_app" => format!("open {}", s("target")),
        "git" => format!("git {}", one_line(s("args"), 100)),
        "web_fetch" => format!("fetch {}", s("url")),
        "use_skill" => format!("load skill {}", s("name")),
        "spawn_subagent" => format!("subagent {}", s("name")),
        "finish" => "finish".to_string(),
        other => other.to_string(),
    }
}

pub fn execute(ctx: &ToolContext, name: &str, args: &Value, interrupt: &AtomicBool) -> ToolOutcome {
    let result = match name {
        "read_file" => read_file(ctx, args),
        "write_file" => write_file(ctx, args),
        "edit_file" => edit_file(ctx, args),
        "move_file" => move_file(ctx, args),
        "delete_file" => delete_file(ctx, args),
        "make_dir" => make_dir(ctx, args),
        "list_dir" => list_dir(ctx, args),
        "glob" => glob(ctx, args),
        "grep" => grep(ctx, args),
        "run_command" => run_command(ctx, args, interrupt),
        "install_package" => install_package(ctx, args, interrupt),
        "open_app" => open_app(args),
        "git" => git(ctx, args, interrupt),
        "web_fetch" => web_fetch(args, interrupt),
        "use_skill" => use_skill(ctx, args),
        "finish" => ToolOutcome::ok(args["summary"].as_str().unwrap_or("done")),
        "spawn_subagent" => ToolOutcome::err("spawn_subagent is handled by the agent loop"),
        other => ToolOutcome::err(format!("unknown tool `{other}`")),
    };
    result
}

/// A diff preview for a write or edit, so Ask mode can show what will change
/// before it changes. `None` for tools that cannot be previewed cheaply.
pub fn preview(ctx: &ToolContext, name: &str, args: &Value) -> Option<String> {
    match name {
        "write_file" => {
            let path = resolve(&ctx.root, args["path"].as_str()?);
            let content = args["content"].as_str().unwrap_or_default();
            let diff = if path.exists() {
                let before = fs::read_to_string(&path).unwrap_or_default();
                unified_diff(&before, content, &path)
            } else {
                new_file_diff(content, &path)
            };
            Some(clip(&diff, 2000))
        }
        "edit_file" => {
            let path = resolve(&ctx.root, args["path"].as_str()?);
            let old = args["old_string"].as_str().unwrap_or_default();
            let new = args["new_string"].as_str().unwrap_or_default();
            let before = fs::read_to_string(&path).ok()?;
            let after = if args["replace_all"].as_bool().unwrap_or(false) {
                before.replace(old, new)
            } else {
                before.replacen(old, new, 1)
            };
            Some(clip(&unified_diff(&before, &after, &path), 2000))
        }
        "delete_file" | "move_file" | "install_package" => Some(summarize(name, args)),
        _ => None,
    }
}

pub fn resolve(root: &Path, path: &str) -> PathBuf {
    let raw = PathBuf::from(path.trim());
    let joined = if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    };
    lexical_normalize(&joined)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn read_file(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) => resolve(&ctx.root, p),
        None => return ToolOutcome::err("read_file requires `path`"),
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => return ToolOutcome::err(format!("cannot read {}: {err}", path.display())),
    };
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(4000) as usize;
    let mut out = String::new();
    let mut shown = 0usize;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if number < offset {
            continue;
        }
        if shown >= limit {
            out.push_str(&format!(
                "… ({} more lines)\n",
                text.lines().count().saturating_sub(index)
            ));
            break;
        }
        out.push_str(&format!("{number:>5}  {line}\n"));
        shown += 1;
        if out.len() > MAX_READ_CHARS {
            out.push_str("… (truncated)\n");
            break;
        }
    }
    if out.is_empty() {
        out.push_str("(empty file)\n");
    }
    ToolOutcome::ok(out)
}

fn write_file(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) => resolve(&ctx.root, p),
        None => return ToolOutcome::err("write_file requires `path`"),
    };
    let content = args["content"].as_str().unwrap_or_default();
    let existed = path.exists();
    let previous = if existed {
        fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return ToolOutcome::err(format!("cannot create {}: {err}", parent.display()));
        }
    }
    if let Err(err) = write_atomic(&path, content) {
        return ToolOutcome::err(format!("cannot write {}: {err}", path.display()));
    }
    let diff = if existed {
        unified_diff(&previous, content, &path)
    } else {
        new_file_diff(content, &path)
    };
    ToolOutcome::ok(format!(
        "{} {} ({} bytes)",
        if existed { "updated" } else { "created" },
        display_path(&ctx.root, &path),
        content.len()
    ))
    .with_diff(diff)
}

fn edit_file(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) => resolve(&ctx.root, p),
        None => return ToolOutcome::err("edit_file requires `path`"),
    };
    let old = args["old_string"].as_str().unwrap_or_default();
    let new = args["new_string"].as_str().unwrap_or_default();
    if old.is_empty() {
        return ToolOutcome::err("edit_file `old_string` must not be empty");
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => return ToolOutcome::err(format!("cannot read {}: {err}", path.display())),
    };
    let occurrences = text.matches(old).count();
    if occurrences == 0 {
        return ToolOutcome::err(format!(
            "`old_string` not found in {}. Read the file and use an exact excerpt.",
            display_path(&ctx.root, &path)
        ));
    }
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    if occurrences > 1 && !replace_all {
        return ToolOutcome::err(format!(
            "`old_string` appears {occurrences} times in {}. Add surrounding context or set replace_all.",
            display_path(&ctx.root, &path)
        ));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    if let Err(err) = write_atomic(&path, &updated) {
        return ToolOutcome::err(format!("cannot write {}: {err}", path.display()));
    }
    ToolOutcome::ok(format!(
        "edited {} ({occurrences} replacement{})",
        display_path(&ctx.root, &path),
        if occurrences == 1 { "" } else { "s" }
    ))
    .with_diff(unified_diff(&text, &updated, &path))
}

fn move_file(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let (Some(from), Some(to)) = (args["from"].as_str(), args["to"].as_str()) else {
        return ToolOutcome::err("move_file requires `from` and `to`");
    };
    let from = resolve(&ctx.root, from);
    let to = resolve(&ctx.root, to);
    if !from.exists() {
        return ToolOutcome::err(format!("{} does not exist", from.display()));
    }
    if to.exists() {
        return ToolOutcome::err(format!("{} already exists", to.display()));
    }
    if let Some(parent) = to.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return ToolOutcome::err(format!("cannot create {}: {err}", parent.display()));
        }
    }
    match fs::rename(&from, &to) {
        Ok(_) => ToolOutcome::ok(format!(
            "moved {} to {}",
            display_path(&ctx.root, &from),
            display_path(&ctx.root, &to)
        )),
        Err(err) => ToolOutcome::err(format!("cannot move: {err}")),
    }
}

fn delete_file(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) => resolve(&ctx.root, p),
        None => return ToolOutcome::err("delete_file requires `path`"),
    };
    if !path.exists() {
        return ToolOutcome::err(format!("{} does not exist", path.display()));
    }
    if path.is_dir() {
        return match fs::remove_dir(&path) {
            Ok(_) => ToolOutcome::ok(format!(
                "removed directory {}",
                display_path(&ctx.root, &path)
            )),
            Err(err) => ToolOutcome::err(format!(
                "cannot remove {}: {err}. Remove its contents first.",
                path.display()
            )),
        };
    }
    match fs::remove_file(&path) {
        Ok(_) => ToolOutcome::ok(format!("deleted {}", display_path(&ctx.root, &path))),
        Err(err) => ToolOutcome::err(format!("cannot delete {}: {err}", path.display())),
    }
}

fn make_dir(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) if !p.trim().is_empty() => resolve(&ctx.root, p),
        _ => return ToolOutcome::err("make_dir requires `path`"),
    };
    match fs::create_dir_all(&path) {
        Ok(_) => ToolOutcome::ok(format!("created {}", display_path(&ctx.root, &path))),
        Err(err) => ToolOutcome::err(format!("cannot create {}: {err}", path.display())),
    }
}

/// Write through a temporary file so a crash cannot leave a half-written file.
/// Existing permissions (such as `+x`) are preserved.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".hasty-tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content.as_bytes())?;
        let _ = file.sync_all();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let _ =
                fs::set_permissions(&temp, fs::Permissions::from_mode(meta.permissions().mode()));
        }
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

fn list_dir(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let path = match args["path"].as_str() {
        Some(p) if !p.trim().is_empty() => resolve(&ctx.root, p),
        _ => ctx.root.clone(),
    };
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(err) => return ToolOutcome::err(format!("cannot list {}: {err}", path.display())),
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let mut out = String::new();
    for name in dirs.iter().chain(files.iter()).take(500) {
        out.push_str(name);
        out.push('\n');
    }
    if dirs.len() + files.len() > 500 {
        out.push_str("… (truncated)\n");
    }
    if out.is_empty() {
        out.push_str("(empty directory)\n");
    }
    ToolOutcome::ok(out)
}

fn glob(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let pattern = match args["pattern"].as_str() {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => return ToolOutcome::err("glob requires `pattern`"),
    };
    let base = args["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .map(|p| resolve(&ctx.root, p))
        .unwrap_or_else(|| ctx.root.clone());
    let regex = match glob_regex(&pattern) {
        Ok(regex) => regex,
        Err(err) => return ToolOutcome::err(err),
    };
    let rules = IgnoreRules::load(&ctx.root);
    let mut matches = Vec::new();
    walk(&ctx.root, &base, &rules, &mut |path| {
        if matches.len() >= 500 {
            return;
        }
        let relative = path
            .strip_prefix(&ctx.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if regex.is_match(&relative) || regex.is_match(&name) {
            matches.push(relative);
        }
    });
    matches.sort();
    if matches.is_empty() {
        return ToolOutcome::ok("no files matched");
    }
    ToolOutcome::ok(matches.join("\n"))
}

fn grep(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let pattern = match args["pattern"].as_str() {
        Some(p) if !p.trim().is_empty() => p,
        _ => return ToolOutcome::err("grep requires `pattern`"),
    };
    let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
    let context = args["context"].as_u64().unwrap_or(0).min(10) as usize;
    let regex = match regex::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(regex) => regex,
        Err(err) => return ToolOutcome::err(format!("invalid regex: {err}")),
    };
    let base = args["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .map(|p| resolve(&ctx.root, p))
        .unwrap_or_else(|| ctx.root.clone());
    let filter = match args["glob"].as_str().filter(|g| !g.trim().is_empty()) {
        Some(glob) => match glob_regex(glob) {
            Ok(regex) => Some(regex),
            Err(err) => return ToolOutcome::err(err),
        },
        None => None,
    };
    let rules = IgnoreRules::load(&ctx.root);
    let mut out = String::new();
    let mut hits = 0usize;
    let mut files_scanned = 0usize;
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        if hits >= 200 || files_scanned > 4000 {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let relative = path
                .strip_prefix(&ctx.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if should_skip_dir(&name) || rules.ignored(&relative, true) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                if should_skip_file(&name) || rules.ignored(&relative, false) {
                    continue;
                }
                if let Some(filter) = &filter {
                    if !filter.is_match(&relative) && !filter.is_match(&name) {
                        continue;
                    }
                }
                files_scanned += 1;
                let text = match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                if text.len() > 1_500_000 || text.contains('\u{0}') {
                    continue;
                }
                let lines: Vec<&str> = text.lines().collect();
                let mut last_printed = 0usize;
                for (index, line) in lines.iter().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    let start = index.saturating_sub(context).max(last_printed);
                    let end = (index + context + 1).min(lines.len());
                    for (offset, body) in lines.iter().enumerate().take(end).skip(start) {
                        let marker = if offset == index { ':' } else { '-' };
                        out.push_str(&format!(
                            "{}:{}{} {}\n",
                            relative,
                            offset + 1,
                            marker,
                            one_line(body.trim(), 240)
                        ));
                        hits += 1;
                    }
                    last_printed = end;
                    if hits >= 200 {
                        break;
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return ToolOutcome::ok("no matches");
    }
    if hits >= 200 {
        out.push_str("… (truncated at 200 matches)\n");
    }
    ToolOutcome::ok(out)
}

fn run_command(ctx: &ToolContext, args: &Value, interrupt: &AtomicBool) -> ToolOutcome {
    let command = match args["command"].as_str() {
        Some(c) if !c.trim().is_empty() => c,
        _ => return ToolOutcome::err("run_command requires `command`"),
    };
    let cwd = args["cwd"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .map(|c| resolve(&ctx.root, c))
        .unwrap_or_else(|| ctx.root.clone());
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 3600);
    match shell(command, &cwd, Duration::from_secs(timeout), interrupt) {
        Ok(result) => {
            let mut out = String::new();
            if !result.stdout.trim().is_empty() {
                out.push_str(&clip(&result.stdout, MAX_OUTPUT_CHARS));
            }
            if !result.stderr.trim().is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("stderr:\n");
                out.push_str(&clip(&result.stderr, MAX_OUTPUT_CHARS));
            }
            if result.timed_out {
                out.push_str(&format!("\n(timed out after {timeout}s)"));
            }
            if out.trim().is_empty() {
                out.push_str(&format!("(no output, exit code {})", result.code));
            }
            out.push_str(&format!("\nexit code: {}", result.code));
            if result.code != 0 || result.timed_out {
                ToolOutcome::err(out)
            } else {
                ToolOutcome::ok(out)
            }
        }
        Err(err) => ToolOutcome::err(err),
    }
}

pub struct ShellResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
}

pub fn shell(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    interrupt: &AtomicBool,
) -> Result<ShellResult, String> {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        // Respect the user's shell when it is a POSIX-compatible one; a zsh or
        // fish user expects their own syntax to work.
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| {
                        matches!(name.to_str(), Some("sh" | "bash" | "zsh" | "dash" | "fish"))
                    })
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| "sh".to_string());
        let mut c = Command::new(shell);
        c.arg("-c").arg(command);
        // Own process group, so a timeout kills grandchildren (dev servers, npm
        // scripts) instead of orphaning them.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        c
    };

    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start command: {e}"))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let started = Instant::now();
    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() > timeout {
                    timed_out = true;
                    kill_tree(&mut child);
                    break child.wait().ok();
                }
                if interrupt.load(Ordering::SeqCst) {
                    interrupted = true;
                    kill_tree(&mut child);
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(err) => return Err(format!("command failed: {err}")),
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    let code = status
        .and_then(|s| s.code())
        .unwrap_or(if timed_out || interrupted { 124 } else { 1 });
    if interrupted {
        return Err("command interrupted".into());
    }
    Ok(ShellResult {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        code,
        timed_out,
    })
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Negative pid targets the whole process group created by setsid().
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn install_package(ctx: &ToolContext, args: &Value, interrupt: &AtomicBool) -> ToolOutcome {
    let package = match args["package"].as_str() {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return ToolOutcome::err("install_package requires `package`"),
    };
    let explicit = args["manager"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // The package string becomes part of a shell command; allow only the
    // characters real package names and version specifiers use.
    let valid = !package.is_empty()
        && package.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | '@' | ':' | '+' | '=' | '~' | '^')
        });
    if !valid {
        return ToolOutcome::err(format!(
            "`{package}` is not a valid package name. Pass one package, without shell syntax."
        ));
    }
    let real = detect_install(&ctx.root, package, &explicit);
    if real.is_empty() {
        return ToolOutcome::err(
            "could not detect a package manager here. Pass `manager` explicitly.",
        );
    }
    match shell(&real, &ctx.root, Duration::from_secs(600), interrupt) {
        Ok(result) => {
            let mut out = String::new();
            out.push_str(&format!("$ {real}\n"));
            out.push_str(&clip(&result.stdout, 8000));
            if !result.stderr.trim().is_empty() {
                out.push('\n');
                out.push_str(&clip(&result.stderr, 8000));
            }
            if result.code == 0 {
                ToolOutcome::ok(out)
            } else {
                ToolOutcome::err(out)
            }
        }
        Err(err) => ToolOutcome::err(err),
    }
}

fn detect_install(root: &Path, package: &str, explicit: &str) -> String {
    let has = |name: &str| root.join(name).exists();
    let manager = if !explicit.is_empty() {
        explicit.to_string()
    } else if has("Cargo.toml") {
        "cargo".into()
    } else if has("package.json") {
        if has("pnpm-lock.yaml") {
            "pnpm".into()
        } else if has("yarn.lock") {
            "yarn".into()
        } else if has("bun.lockb") || has("bun.lock") {
            "bun".into()
        } else {
            "npm".into()
        }
    } else if has("pyproject.toml") || has("requirements.txt") {
        if has("uv.lock") {
            "uv".into()
        } else {
            "pip".into()
        }
    } else if has("go.mod") {
        "go".into()
    } else if has("Gemfile") {
        "bundle".into()
    } else {
        return String::new();
    };
    match manager.as_str() {
        "npm" => format!("npm install {package}"),
        "pnpm" => format!("pnpm add {package}"),
        "yarn" => format!("yarn add {package}"),
        "bun" => format!("bun add {package}"),
        "cargo" => format!("cargo add {package}"),
        "pip" => format!("pip install {package}"),
        "uv" => format!("uv add {package}"),
        "go" => format!("go get {package}"),
        "bundle" => format!("bundle add {package}"),
        _ => String::new(),
    }
}

fn open_app(args: &Value) -> ToolOutcome {
    let target = match args["target"].as_str() {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return ToolOutcome::err("open_app requires `target`"),
    };
    #[cfg(target_os = "macos")]
    let (program, arguments) = ("open", vec![target.to_string()]);
    #[cfg(target_os = "windows")]
    let (program, arguments) = (
        "cmd",
        vec![
            "/C".to_string(),
            "start".to_string(),
            String::new(),
            target.to_string(),
        ],
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, arguments) = ("xdg-open", vec![target.to_string()]);

    match Command::new(program)
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => ToolOutcome::ok(format!("opened {target}")),
        Err(err) => ToolOutcome::err(format!("cannot open {target}: {err}")),
    }
}

fn git(ctx: &ToolContext, args: &Value, interrupt: &AtomicBool) -> ToolOutcome {
    let git_args = args["args"].as_str().unwrap_or_default();
    if git_args.contains(';') || git_args.contains('|') || git_args.contains('&') {
        return ToolOutcome::err("git tool takes plain git arguments only");
    }
    let command = format!("git {git_args}");
    let mut outcome = run_command(
        ctx,
        &json!({"command": command, "timeout_secs": 180}),
        interrupt,
    );
    if outcome.ok && git_args.trim_start().starts_with("status") {
        outcome
            .output
            .push_str("\nreminder: never commit unless the user asked.\n");
    }
    outcome
}

fn web_fetch(args: &Value, interrupt: &AtomicBool) -> ToolOutcome {
    let url = match args["url"].as_str() {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u,
        _ => return ToolOutcome::err("web_fetch requires an http(s) URL"),
    };
    if interrupt.load(Ordering::SeqCst) {
        return ToolOutcome::err("interrupted");
    }
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(format!("hasty/{}", crate::VERSION))
        .build()
    {
        Ok(client) => client,
        Err(err) => return ToolOutcome::err(format!("http client failed: {err}")),
    };
    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(err) => return ToolOutcome::err(format!("fetch failed: {err}")),
    };
    let status = response.status();
    if !status.is_success() {
        return ToolOutcome::err(format!("{url} returned HTTP {}", status.as_u16()));
    }
    if let Some(kind) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        let kind = kind.to_ascii_lowercase();
        if kind.starts_with("image/")
            || kind.starts_with("audio/")
            || kind.starts_with("video/")
            || kind.contains("octet-stream")
            || kind.starts_with("application/pdf")
        {
            return ToolOutcome::err(format!(
                "{url} is binary ({kind}); web_fetch only reads text pages"
            ));
        }
    }
    let body = match response.text() {
        Ok(body) => body,
        Err(err) => return ToolOutcome::err(format!("could not read body: {err}")),
    };
    let text = if body.contains("<html") || body.contains("<HTML") {
        html_to_text(&body)
    } else {
        body
    };
    ToolOutcome::ok(clip(&text, 60_000))
}

pub fn html_to_text(html: &str) -> String {
    // The regex crate has no backreferences, so script and style are stripped
    // with two separate patterns.
    let script = Regex::new(r"(?is)<script[^>]*>.*?</script>").expect("static regex");
    let style = Regex::new(r"(?is)<style[^>]*>.*?</style>").expect("static regex");
    let tags = Regex::new(r"(?s)<[^>]+>").expect("static regex");
    let spaces = Regex::new(r"[ \t]{2,}").expect("static regex");
    let blank_lines = Regex::new(r"\n{3,}").expect("static regex");
    let text = script.replace_all(html, " ");
    let text = style.replace_all(&text, " ");
    let text = tags.replace_all(&text, "\n");
    let text = spaces.replace_all(&text, " ");
    let text = blank_lines.replace_all(&text, "\n\n");
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    decoded
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn use_skill(ctx: &ToolContext, args: &Value) -> ToolOutcome {
    let name = match args["name"].as_str() {
        Some(n) if !n.trim().is_empty() => n,
        _ => return ToolOutcome::err("use_skill requires `name`"),
    };
    match crate::skills::load_in(Some(&ctx.root), name) {
        Ok(content) => ToolOutcome::ok(content),
        Err(err) => ToolOutcome::err(err),
    }
}

// --------------------------------------------------------------------- helpers

fn walk(root: &Path, start: &Path, rules: &IgnoreRules, visit: &mut dyn FnMut(&Path)) {
    let mut stack = vec![start.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        if seen > 20_000 {
            return;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            seen += 1;
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if !should_skip_dir(&name) && !rules.ignored(&relative, true) {
                    stack.push(path);
                }
            } else if file_type.is_file()
                && !should_skip_file(&name)
                && !rules.ignored(&relative, false)
            {
                visit(&path);
            }
        }
    }
}

/// Patterns from `.gitignore`, `.hastyignore` and `.ignore` at the project root.
/// Name rules match any component; path rules are matched as globs.
pub struct IgnoreRules {
    names: Vec<Regex>,
    dirs: Vec<String>,
    globs: Vec<Regex>,
}

impl IgnoreRules {
    pub fn load(root: &Path) -> Self {
        let mut names = Vec::new();
        let mut dirs = Vec::new();
        let mut globs = Vec::new();
        for file in [".gitignore", ".hastyignore", ".ignore"] {
            let Ok(text) = fs::read_to_string(root.join(file)) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                let line = line.trim_start_matches('/').trim_start_matches("**/");
                if line.ends_with('/') {
                    dirs.push(line.trim_end_matches('/').to_string());
                } else if line.contains('/') {
                    if let Ok(regex) = glob_regex(line) {
                        globs.push(regex);
                    }
                } else if let Ok(regex) = glob_regex(line) {
                    names.push(regex);
                }
            }
        }
        Self { names, dirs, globs }
    }

    pub fn ignored(&self, relative: &str, _is_dir: bool) -> bool {
        let relative = relative.trim_start_matches("./").trim_matches('/');
        if relative.is_empty() {
            return false;
        }
        let name = relative.rsplit('/').next().unwrap_or(relative);
        if self.names.iter().any(|rule| rule.is_match(name)) {
            return true;
        }
        if self.dirs.iter().any(|dir| {
            relative == dir
                || relative.starts_with(&format!("{dir}/"))
                || relative.contains(&format!("/{dir}/"))
        }) {
            return true;
        }
        self.globs.iter().any(|regex| regex.is_match(relative))
    }
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "out"
            | ".next"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".idea"
            | ".cache"
            | "vendor"
    )
}

fn should_skip_file(name: &str) -> bool {
    matches!(
        name,
        ".DS_Store" | "hasty.db" | "hasty.db-wal" | "hasty.db-shm"
    )
}

fn glob_regex(pattern: &str) -> Result<Regex, String> {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.replace('\\', "/").chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                    if i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                    continue;
                }
                out.push_str("[^/]*");
            }
            '?' => out.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
        i += 1;
    }
    out.push('$');
    Regex::new(&out).map_err(|e| format!("invalid glob: {e}"))
}

fn unified_diff(before: &str, after: &str, path: &Path) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut out = String::new();
    for group in diff.grouped_ops(3) {
        let first = group.first().map(|op| op.old_range().start).unwrap_or(0);
        let last = group.last().map(|op| op.old_range().end).unwrap_or(0);
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            first + 1,
            last - first,
            first + 1,
            last - first
        ));
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                    similar::ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(change.value().trim_end_matches('\n'));
                out.push('\n');
            }
        }
    }
    if out.is_empty() {
        format!("(no change to {})", path.display())
    } else {
        out
    }
}

fn new_file_diff(content: &str, path: &Path) -> String {
    let mut out = format!("+++ {}", path.display());
    out.push('\n');
    for line in content.lines().take(200) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if content.lines().count() > 200 {
        out.push_str("… (truncated)\n");
    }
    out
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn clip(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    // Keep both ends: compiler and test failures live at the end of the output,
    // while the command and the first errors live at the start.
    let head = max / 2;
    let tail = max.saturating_sub(head + 32);
    let mut out: String = chars[..head].iter().collect();
    out.push_str(&format!(
        "\n… ({} characters elided) …\n",
        chars.len() - head - tail
    ));
    out.extend(chars[chars.len() - tail..].iter());
    out
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max).collect::<String>() + "…"
    } else {
        flat
    }
}

/// Write a small file, used by /init and tests.
pub fn write_text(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_ctx() -> (ToolContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hasty-tools-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("src")).unwrap();
        (ToolContext::new(dir.clone()), dir)
    }

    #[test]
    fn write_then_read_and_edit() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        let out = execute(
            &ctx,
            "write_file",
            &json!({"path": "src/main.rs", "content": "fn main() {\n    println!(\"hi\");\n}\n"}),
            &flag,
        );
        assert!(out.ok, "{}", out.output);

        let out = execute(&ctx, "read_file", &json!({"path": "src/main.rs"}), &flag);
        assert!(out.output.contains("println!"));

        let out = execute(
            &ctx,
            "edit_file",
            &json!({"path": "src/main.rs", "old_string": "hi", "new_string": "hello"}),
            &flag,
        );
        assert!(out.ok, "{}", out.output);
        assert!(out.diff.unwrap().contains("+    println!(\"hello\")"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_rejects_missing_string() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        execute(
            &ctx,
            "write_file",
            &json!({"path": "a.txt", "content": "abc"}),
            &flag,
        );
        let out = execute(
            &ctx,
            "edit_file",
            &json!({"path": "a.txt", "old_string": "zzz", "new_string": "x"}),
            &flag,
        );
        assert!(!out.ok);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_matches_nested_files() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        execute(
            &ctx,
            "write_file",
            &json!({"path": "src/a.rs", "content": "x"}),
            &flag,
        );
        execute(
            &ctx,
            "write_file",
            &json!({"path": "src/deep/b.rs", "content": "x"}),
            &flag,
        );
        let out = execute(&ctx, "glob", &json!({"pattern": "src/**/*.rs"}), &flag);
        assert!(out.output.contains("src/a.rs"), "{}", out.output);
        assert!(out.output.contains("src/deep/b.rs"), "{}", out.output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn shell_reports_exit_code() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        let out = execute(&ctx, "run_command", &json!({"command": "exit 3"}), &flag);
        assert!(!out.ok);
        assert!(out.output.contains("exit code: 3"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn strip_html() {
        let text =
            html_to_text("<html><style>a{}</style><body><h1>Hi</h1><p>there</p></body></html>");
        assert!(text.contains("Hi"));
        assert!(text.contains("there"));
        assert!(!text.contains("<h1>"));
    }

    #[test]
    fn write_move_delete_round_trip() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        assert!(
            execute(
                &ctx,
                "write_file",
                &json!({"path": "a/b.txt", "content": "hello"}),
                &flag
            )
            .ok
        );
        assert!(
            execute(
                &ctx,
                "move_file",
                &json!({"from": "a/b.txt", "to": "a/c.txt"}),
                &flag
            )
            .ok
        );
        assert!(dir.join("a/c.txt").exists());
        assert!(execute(&ctx, "delete_file", &json!({"path": "a/c.txt"}), &flag).ok);
        assert!(!dir.join("a/c.txt").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let (ctx, dir) = temp_ctx();
        let path = dir.join("run.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let flag = AtomicBool::new(false);
        execute(
            &ctx,
            "write_file",
            &json!({"path": "run.sh", "content": "#!/bin/sh\necho hi\n"}),
            &flag,
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn clip_keeps_the_end_where_failures_live() {
        let text = format!("{}{}", "a".repeat(500), "THE-ERROR-AT-THE-END");
        let out = clip(&text, 200);
        assert!(out.contains("THE-ERROR-AT-THE-END"));
        assert!(out.contains("elided"));
    }

    #[test]
    fn ignore_files_are_respected() {
        let (ctx, dir) = temp_ctx();
        fs::write(dir.join(".gitignore"), "generated/\n*.log\n").unwrap();
        fs::create_dir_all(dir.join("generated")).unwrap();
        fs::write(dir.join("generated/x.rs"), "x").unwrap();
        fs::write(dir.join("keep.rs"), "x").unwrap();
        fs::write(dir.join("debug.log"), "x").unwrap();
        let flag = AtomicBool::new(false);
        let out = execute(&ctx, "glob", &json!({"pattern": "**/*"}), &flag);
        assert!(out.output.contains("keep.rs"), "{}", out.output);
        assert!(!out.output.contains("generated/x.rs"), "{}", out.output);
        assert!(!out.output.contains("debug.log"), "{}", out.output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn install_rejects_shell_syntax_in_package_names() {
        let (ctx, dir) = temp_ctx();
        let flag = AtomicBool::new(false);
        let out = execute(
            &ctx,
            "install_package",
            &json!({"package": "left-pad; touch pwned"}),
            &flag,
        );
        assert!(!out.ok);
        let _ = fs::remove_dir_all(dir);
    }
}
