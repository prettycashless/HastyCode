//! Slash-command metadata and helpers that need no UI (project notes, export).

use std::fs;
use std::path::{Path, PathBuf};

use crate::db::Store;

#[derive(Debug, Clone, Copy)]
pub struct CommandInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

pub fn all() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/help",
            args: "",
            description: "Keys, commands and mouse hints",
        },
        CommandInfo {
            name: "/model",
            args: "",
            description: "Pick provider and main model",
        },
        CommandInfo {
            name: "/theme",
            args: "[name|set <slot> <#hex>]",
            description: "Preset or custom colors",
        },
        CommandInfo {
            name: "/subagents",
            args: "",
            description: "Running subagents and their keys",
        },
        CommandInfo {
            name: "/settings",
            args: "",
            description: "Permissions, keys, connectors, paths",
        },
        CommandInfo {
            name: "/skills",
            args: "[install <url>|remove <repo>]",
            description: "Install and enable skills",
        },
        CommandInfo {
            name: "/connectors",
            args: "",
            description: "GitHub and other connectors",
        },
        CommandInfo {
            name: "/sessions",
            args: "",
            description: "Browse, resume or delete sessions",
        },
        CommandInfo {
            name: "/clear",
            args: "",
            description: "Clear the conversation context",
        },
        CommandInfo {
            name: "/compact",
            args: "",
            description: "Summarize the context to save tokens",
        },
        CommandInfo {
            name: "/init",
            args: "",
            description: "Write project notes (HASTY.md)",
        },
        CommandInfo {
            name: "/exit",
            args: "",
            description: "Leave Hasty",
        },
    ]
}

pub fn matching(prefix: &str) -> Vec<CommandInfo> {
    let prefix = prefix.trim_start_matches('/');
    all()
        .into_iter()
        .filter(|cmd| cmd.name.trim_start_matches('/').starts_with(prefix))
        .collect()
}

pub fn parse(line: &str) -> (String, String) {
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_string();
    let rest = parts.next().unwrap_or("").trim().to_string();
    (cmd, rest)
}

/// Generate a starting `HASTY.md` without calling a model.
pub fn generate_project_notes(root: &Path) -> String {
    let mut out = String::new();
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    out.push_str(&format!("# {name} — notes for Hasty\n\n"));
    out.push_str(
        "<!-- Written by /init. Edit freely; Hasty reads this file as project context. -->\n\n",
    );

    // Overview from README.
    if let Some(readme) = ["README.md", "README", "readme.md"]
        .iter()
        .map(|f| root.join(f))
        .find(|p| p.exists())
    {
        if let Ok(text) = fs::read_to_string(&readme) {
            let overview = first_meaningful_paragraph(&text);
            if !overview.is_empty() {
                out.push_str("## Overview\n");
                out.push_str(&overview);
                out.push_str("\n\n");
            }
        }
    }

    // Stack detection.
    let mut stack = Vec::new();
    if root.join("Cargo.toml").exists() {
        stack.push("Rust (Cargo)");
    }
    if root.join("package.json").exists() {
        stack.push("Node/JavaScript packages");
    }
    if root.join("tsconfig.json").exists() {
        stack.push("TypeScript");
    }
    if root.join("pyproject.toml").exists() || root.join("requirements.txt").exists() {
        stack.push("Python");
    }
    if root.join("go.mod").exists() {
        stack.push("Go");
    }
    if root.join("Gemfile").exists() {
        stack.push("Ruby");
    }
    if root.join("pom.xml").exists() || root.join("build.gradle").exists() {
        stack.push("Java/Kotlin");
    }
    if !stack.is_empty() {
        out.push_str("## Stack\n");
        for item in stack {
            out.push_str(&format!("- {item}\n"));
        }
        out.push('\n');
    }

    // Commands.
    let mut commands: Vec<(String, String)> = Vec::new();
    if let Ok(text) = fs::read_to_string(root.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(scripts) = json["scripts"].as_object() {
                let manager = if root.join("pnpm-lock.yaml").exists() {
                    "pnpm"
                } else if root.join("yarn.lock").exists() {
                    "yarn"
                } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
                    "bun"
                } else {
                    "npm"
                };
                for (key, value) in scripts {
                    let command = match manager {
                        "npm" => format!("npm run {key}"),
                        other => format!("{other} {key}"),
                    };
                    commands.push((command, value.as_str().unwrap_or("").to_string()));
                }
            }
        }
    }
    if root.join("Cargo.toml").exists() {
        commands.push(("cargo build".into(), "compile".into()));
        commands.push(("cargo test".into(), "run tests".into()));
        commands.push((
            "cargo clippy --all-targets -- -D warnings".into(),
            "lint".into(),
        ));
    }
    if root.join("Makefile").exists() {
        if let Ok(text) = fs::read_to_string(root.join("Makefile")) {
            for line in text.lines() {
                if let Some((target, _)) = line.split_once(':') {
                    let target = target.trim();
                    if !target.is_empty()
                        && !target.contains(' ')
                        && !target.starts_with('.')
                        && !target.starts_with('#')
                    {
                        commands.push((format!("make {target}"), "Makefile target".into()));
                    }
                }
            }
        }
    }
    if !commands.is_empty() {
        out.push_str("## Commands\n");
        for (command, note) in commands.iter().take(20) {
            out.push_str(&format!("- `{command}` — {note}\n"));
        }
        out.push('\n');
    }

    // Layout.
    out.push_str("## Layout\n");
    for line in tree(root, 2, 40) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');

    // Verification guidance.
    out.push_str("## Verification\n");
    let mut verify = Vec::new();
    if root.join("Cargo.toml").exists() {
        verify.push("`cargo test`");
        verify.push("`cargo build`");
    }
    if root.join("package.json").exists() {
        verify.push("the project's `test` script");
        verify.push("the project's `build` script");
    }
    if verify.is_empty() {
        out.push_str("- Add the exact commands that prove a change works.\n");
    } else {
        out.push_str(&format!(
            "- Run {} before declaring work done.\n",
            verify.join(" and ")
        ));
    }
    out.push('\n');

    out.push_str("## Conventions\n");
    out.push_str("- Match the surrounding style. Do not reformat unrelated code.\n");
    out.push_str("- Keep changes small and focused; verify before finishing.\n");
    out
}

fn first_meaningful_paragraph(text: &str) -> String {
    let mut collected = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !collected.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with('#')
            || line.starts_with('[')
            || line.starts_with("![")
            || line.starts_with("<img")
            || line.starts_with("```")
        {
            continue;
        }
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(line);
        if collected.chars().count() > 600 {
            break;
        }
    }
    let chars: Vec<char> = collected.chars().collect();
    if chars.len() > 800 {
        chars[..800].iter().collect::<String>() + "…"
    } else {
        collected
    }
}

fn tree(root: &Path, depth: usize, limit: usize) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, depth: usize, limit: usize, out: &mut Vec<String>) {
        if out.len() >= limit || depth == 0 {
            return;
        }
        let mut entries: Vec<_> = match fs::read_dir(dir) {
            Ok(entries) => entries.flatten().collect(),
            Err(_) => return,
        };
        entries.sort_by_key(|e| {
            (
                !e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                e.file_name(),
            )
        });
        for entry in entries {
            if out.len() >= limit {
                return;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.')
                || matches!(name.as_str(), "node_modules" | "target" | "dist" | "build")
            {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(format!("{prefix}{name}{}", if is_dir { "/" } else { "" }));
            if is_dir {
                walk(&entry.path(), &format!("{prefix}  "), depth - 1, limit, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", depth, limit, &mut out);
    out
}

/// Export one session (or all) to a JSON file.
pub fn export_session(
    store: &Store,
    out: Option<&Path>,
    session: Option<&str>,
) -> Result<PathBuf, String> {
    let json = store
        .export_json(session)
        .map_err(|e| format!("export failed: {e}"))?;
    let path = out
        .map(|p| p.to_path_buf())
        .unwrap_or_else(crate::db::default_export_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::write(&path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse("/theme ember"), ("/theme".into(), "ember".into()));
        assert_eq!(parse("/exit"), ("/exit".into(), "".into()));
    }

    #[test]
    fn matching_commands() {
        let names: Vec<_> = matching("/se").iter().map(|c| c.name).collect();
        assert!(names.contains(&"/sessions"));
        assert!(names.contains(&"/settings"));
    }

    #[test]
    fn notes_include_stack_and_commands() {
        let dir = std::env::temp_dir().join(format!("hasty-notes-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let notes = generate_project_notes(&dir);
        assert!(notes.contains("Rust (Cargo)"));
        assert!(notes.contains("cargo test"));
        assert!(notes.contains("src/"));
        let _ = fs::remove_dir_all(dir);
    }
}
