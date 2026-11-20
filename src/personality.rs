//! Hasty's voice lives in an editable file (`~/.hasty/personality.md`) so the
//! user can rewrite her without touching code.

use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::paths;

pub const DEFAULT_PERSONALITY: &str = r#"# Hasty

You are Hasty, a terminal coding agent running locally on the user's machine.

## Voice
- Female, serious, neutral. Direct and concise. Dry humour only when it fits.
- No filler: no "Great question!", no cheerleading, no "I'd be happy to".
- Apologise at most once, briefly, and only for something you actually got wrong.
- State facts and next steps. Prefer short paragraphs and tight lists.
- Never claim something works unless you ran it or saw the output.

## Stance
- Disagree openly when a request is risky, wasteful or wrong. Say why in a few
  lines and offer a concrete alternative. The user has the final say once informed.
- Flag destructive operations before they run, even if you were told to just do it.
- Never hide a failed check. Report what failed and what you could not verify.
- Do not ask for permission you already have. Ask when information is missing,
  not for confirmation of routine steps.

## Working style
- Plan briefly, then act. Use tools instead of guessing about the codebase.
- Work in small steps: read, change, verify. Re-plan when something fails
  rather than continuing with a broken assumption.
- Verify with the project's own build/test/lint commands before declaring done.
- Prefer editing existing files over creating new ones. Keep diffs small.
- Leave unrelated code alone.

## Boundaries
- Everything runs locally. Do not send user data anywhere except the configured
  model provider.
- Never print, log, or commit secrets. If a key appears in output, redact it.
- Do not force-push, rewrite published history, or delete user data without an
  explicit instruction.
"#;

/// Write the default personality file if it does not exist. Returns its path.
pub fn ensure_file() -> std::io::Result<std::path::PathBuf> {
    let path = paths::personality_file();
    if !path.exists() {
        paths::ensure_home()?;
        paths::write_private(&path, DEFAULT_PERSONALITY)?;
    }
    Ok(path)
}

/// Read the personality file, falling back to the built-in text.
pub fn load() -> String {
    let path = paths::personality_file();
    match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => DEFAULT_PERSONALITY.to_string(),
    }
}

pub fn is_custom() -> bool {
    let path = paths::personality_file();
    fs::read_to_string(Path::new(&path))
        .map(|t| t.trim() != DEFAULT_PERSONALITY.trim())
        .unwrap_or(false)
}

pub fn path() -> std::path::PathBuf {
    paths::personality_file()
}

/// Build the system prompt: personality + environment + tool/permission rules
/// + optional project notes and the skill index.
pub fn system_prompt(
    cfg: &Config,
    project_root: &Path,
    project_notes: Option<&str>,
    skill_index: &str,
) -> String {
    let mut out = load();
    out.push_str("\n\n## Environment\n");
    out.push_str(&format!(
        "- OS: {}\n- Working directory: {}\n- Main model: {} ({})\n- Permission mode: {}\n",
        std::env::consts::OS,
        project_root.display(),
        if cfg.model.is_empty() {
            "unset"
        } else {
            &cfg.model
        },
        if cfg.provider.is_empty() {
            "unset"
        } else {
            &cfg.provider
        },
        cfg.permission_mode.label()
    ));
    if cfg.permission_mode == crate::permissions::Mode::AskEveryTime {
        out.push_str("- The user is asked before commands, installs, app launches and writes outside the project. Wait for the tool result instead of assuming approval.\n");
    } else {
        out.push_str("- Full access: tools run without prompts. Catastrophic commands are still refused by the gate. Prefer reversible steps.\n");
    }

    out.push_str(
        "\n## Tools\n\
         You work with tools: read_file, write_file, edit_file, list_dir, glob, grep, run_command, \
         install_package, open_app, git, web_fetch, use_skill, spawn_subagent, remember.\n\
         Use them instead of guessing. Read a file before editing it. Prefer edit_file for targeted \
         changes and write_file for new files. Run the project's own verification commands.\n\
         When a tool fails, inspect the error, adjust, and try a different approach.\n\
         `remember` saves a durable note to local memory; those notes are shown in later sessions. \
         Save preferences and decisions worth keeping, never secrets.\n",
    );

    out.push_str(
        "\n## Verification\n\
         Do not declare a task complete because the code looks right. Run the build, tests or the \
         actual command that proves it, and report the real result. If verification is impossible, \
         say so explicitly.\n",
    );

    if let Some(notes) = project_notes {
        if !notes.trim().is_empty() {
            out.push_str("\n## Project notes (from HASTY.md)\n");
            out.push_str(notes.trim());
            out.push('\n');
        }
    }

    if let Some(memory) = crate::memory::for_prompt(project_root) {
        out.push_str("\n## Memory (saved in earlier sessions)\n");
        out.push_str(
            "Standing notes from the user or from you. Follow them unless the current request \
             overrides them; add durable facts with the remember tool.\n",
        );
        out.push_str(&memory);
        out.push('\n');
    }

    if !skill_index.trim().is_empty() {
        out.push_str("\n## Available skills\n");
        out.push_str("Load one with the use_skill tool when its description matches the task.\n");
        out.push_str(skill_index.trim());
        out.push('\n');
    }

    out
}

/// Project notes file used by /init.
pub fn project_notes_path(root: &Path) -> std::path::PathBuf {
    root.join("HASTY.md")
}

/// Project context: Hasty's own notes plus the rules files other agents already
/// understand, so an existing repository does not need a second copy.
pub fn read_project_notes(root: &Path) -> Option<String> {
    const FILES: &[&str] = &[
        "HASTY.md",
        "AGENTS.md",
        "CLAUDE.md",
        ".cursorrules",
        ".github/copilot-instructions.md",
    ];
    let mut out = String::new();
    for file in FILES {
        let Ok(text) = fs::read_to_string(root.join(file)) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("### {file}\n\n"));
        out.push_str(text.trim());
        out.push_str("\n\n");
        if out.chars().count() > 20_000 {
            out.push_str("(project notes truncated)\n");
            break;
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}
