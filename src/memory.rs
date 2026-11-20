//! Durable local memory.
//!
//! Hasty saves short notes for itself — decisions, preferences, pitfalls found in
//! a project. They live under `~/.hasty/memory/`, are read back into the system
//! prompt on later sessions, and never leave the machine.

use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

/// How much memory text is injected into a system prompt, newest kept.
pub const PROMPT_LIMIT: usize = 8_000;
/// Hard cap on one memory file; the oldest entries are dropped first.
const FILE_LIMIT: usize = 128_000;
/// One note is clipped to this many characters.
const NOTE_LIMIT: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    pub fn parse(text: &str) -> Scope {
        if text.trim().eq_ignore_ascii_case("global") {
            Scope::Global
        } else {
            Scope::Project
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    New,
    Duplicate,
}

pub fn memory_dir() -> PathBuf {
    paths::home().join("memory")
}

pub fn global_file() -> PathBuf {
    memory_dir().join("global.md")
}

pub fn project_file(root: &Path) -> PathBuf {
    project_file_in(&paths::home(), root)
}

fn project_file_in(base: &Path, root: &Path) -> PathBuf {
    base.join("memory")
        .join("projects")
        .join(format!("{}.md", project_key(root)))
}

/// Stable, filesystem-safe key for a project directory.
fn project_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    let slug: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug: String = if slug.is_empty() {
        "project".into()
    } else {
        slug.chars().take(40).collect()
    };
    format!(
        "{slug}-{:016x}",
        fnv1a(&canonical.to_string_lossy().to_lowercase())
    )
}

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// Save one note. Returns the file it went to and whether it was new.
pub fn remember(root: &Path, text: &str, scope: Scope) -> Result<(PathBuf, Saved), String> {
    remember_in(&paths::home(), root, text, scope)
}

pub fn remember_in(
    base: &Path,
    root: &Path,
    text: &str,
    scope: Scope,
) -> Result<(PathBuf, Saved), String> {
    let note = text.trim();
    if note.is_empty() {
        return Err("Nothing to remember: the note is empty.".into());
    }
    let note = clip(note, NOTE_LIMIT);
    let path = match scope {
        Scope::Project => project_file_in(base, root),
        Scope::Global => base.join("memory").join("global.md"),
    };
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let line = format!(
        "- {} {}",
        date_stamp(crate::config::now_secs()),
        note.replace('\n', " ")
    );
    if existing
        .lines()
        .any(|saved| saved.trim_end() == line.trim_end())
    {
        return Ok((path, Saved::Duplicate));
    }
    let mut body = existing;
    if body.trim().is_empty() {
        body = header(scope, root);
    } else if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&line);
    body.push('\n');
    let body = keep_tail(body, FILE_LIMIT);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    crate::paths::write_private(&path, &body)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((path, Saved::New))
}

fn header(scope: Scope, root: &Path) -> String {
    match scope {
        Scope::Project => format!(
            "# Memory — {}\n\nNotes saved for this project. Edit or delete lines freely.\n\n",
            root.display()
        ),
        Scope::Global => {
            "# Memory — global\n\nNotes saved across every project. Edit or delete lines freely.\n\n"
                .to_string()
        }
    }
}

/// Both memory files with a label, for display.
pub fn files(root: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![("global", global_file()), ("project", project_file(root))]
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// The memory section for the system prompt: global notes then project notes.
pub fn for_prompt(root: &Path) -> Option<String> {
    for_prompt_in(&paths::home(), root)
}

pub fn for_prompt_in(base: &Path, root: &Path) -> Option<String> {
    let mut out = String::new();
    for (label, path) in [
        ("global", base.join("memory").join("global.md")),
        ("project", project_file_in(base, root)),
    ] {
        let text = read(&path);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!("### {label} memory\n"));
        out.push_str(text);
        out.push_str("\n\n");
    }
    if out.trim().is_empty() {
        return None;
    }
    Some(keep_tail(out, PROMPT_LIMIT))
}

/// Keep the end of an over-long body, starting at a line boundary.
fn keep_tail(mut body: String, limit: usize) -> String {
    if body.len() <= limit {
        return body;
    }
    let mut cut = body.len() - limit;
    while cut < body.len() && !body.is_char_boundary(cut) {
        cut += 1;
    }
    if let Some(newline) = body[cut..].find('\n') {
        cut += newline + 1;
    }
    body.drain(..cut);
    body
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn date_stamp(secs: u64) -> String {
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base() -> PathBuf {
        std::env::temp_dir().join(format!("hasty-memory-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn saves_and_reads_project_notes() {
        let base = temp_base();
        let root = base.join("my-project");
        fs::create_dir_all(&root).unwrap();
        let (path, saved) = remember_in(
            &base,
            &root,
            "Use cargo test before finishing.",
            Scope::Project,
        )
        .unwrap();
        assert_eq!(saved, Saved::New);
        assert!(path.starts_with(base.join("memory")));
        let text = read(&path);
        assert!(text.contains("Use cargo test before finishing."));
        assert!(text.contains("my-project"));

        let prompt = for_prompt_in(&base, &root).unwrap();
        assert!(prompt.contains("project memory"));
        assert!(prompt.contains("Use cargo test before finishing."));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn duplicate_notes_are_not_written_twice() {
        let base = temp_base();
        let root = base.join("p");
        fs::create_dir_all(&root).unwrap();
        remember_in(&base, &root, "same note", Scope::Project).unwrap();
        let (_, saved) = remember_in(&base, &root, "same note", Scope::Project).unwrap();
        assert_eq!(saved, Saved::Duplicate);
        let text = read(&project_file_in(&base, &root));
        assert_eq!(text.matches("same note").count(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn global_and_project_are_separate_and_both_prompted() {
        let base = temp_base();
        let root = base.join("p");
        fs::create_dir_all(&root).unwrap();
        remember_in(&base, &root, "global rule", Scope::Global).unwrap();
        remember_in(&base, &root, "project rule", Scope::Project).unwrap();
        let prompt = for_prompt_in(&base, &root).unwrap();
        assert!(prompt.contains("### global memory"));
        assert!(prompt.contains("### project memory"));
        assert!(prompt.contains("global rule"));
        assert!(prompt.contains("project rule"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn empty_note_is_rejected() {
        let base = temp_base();
        assert!(remember_in(&base, &base, "   ", Scope::Project).is_err());
    }

    #[test]
    fn long_histories_keep_the_newest_notes() {
        let base = temp_base();
        let root = base.join("p");
        fs::create_dir_all(&root).unwrap();
        let path = project_file_in(&base, &root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = String::from("# Memory\n\n");
        for index in 0..3_000 {
            body.push_str(&format!(
                "- 1970-01-01 note number {index} with some padding text\n"
            ));
        }
        fs::write(&path, &body).unwrap();
        remember_in(&base, &root, "newest note", Scope::Project).unwrap();
        let text = read(&path);
        assert!(text.len() <= FILE_LIMIT);
        assert!(text.contains("newest note"));
        assert!(!text.contains("note number 0 "));
        let prompt = for_prompt_in(&base, &root).unwrap();
        assert!(prompt.len() <= PROMPT_LIMIT + 64);
        assert!(prompt.contains("newest note"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn project_keys_are_stable_and_safe() {
        let root = Path::new("/tmp/Some Project");
        let key = project_key(root);
        assert!(key.starts_with("some-project-"));
        assert!(!key.contains('/'));
        assert_eq!(key, project_key(root));
    }
}
