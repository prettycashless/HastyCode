//! Skills: portable instruction packs installed from GitHub into
//! `~/.hasty/skills/repos/<owner>__<repo>`.
//!
//! Only the skill index (name + description) is given to the model up front;
//! full instructions are loaded on demand through the `use_skill` tool.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths;

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub repo: String,
    pub name: String,
    pub description: String,
    /// Tools the skill says it needs, when its frontmatter declares them.
    pub allowed_tools: Vec<String>,
    pub path: PathBuf,
}

/// Repos Hasty can suggest. `addyosmani/agent-skills` was verified to exist and
/// to use `skills/<name>/SKILL.md`; the others follow the same convention.
pub fn suggestions() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "addyosmani/agent-skills",
            "https://github.com/addyosmani/agent-skills",
            "Spec, plan, build, test, review and ship workflows (26 skills)",
        ),
        (
            "anthropics/skills",
            "https://github.com/anthropics/skills",
            "Anthropic's reference skills: documents, spreadsheets, artifacts",
        ),
        (
            "obra/superpowers",
            "https://github.com/obra/superpowers",
            "Brainstorming, debugging and verification discipline for coding agents",
        ),
    ]
}

pub const DEFAULT_SUGGESTION: &str = "https://github.com/addyosmani/agent-skills";

pub fn repos_dir() -> PathBuf {
    paths::skills_dir().join("repos")
}

/// Normalize a GitHub URL or `owner/repo` into a clone URL plus a slug.
pub fn parse_source(input: &str) -> Result<(String, String), String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("empty repository".into());
    }
    let trimmed = raw
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("git@");
    let path = if let Some(rest) = trimmed.strip_prefix("github.com:") {
        rest.to_string()
    } else if let Some(rest) = trimmed.strip_prefix("github.com/") {
        rest.to_string()
    } else if trimmed.matches('/').count() == 1 && !trimmed.contains(':') {
        trimmed.to_string()
    } else {
        return Err("provide a GitHub URL like https://github.com/owner/repo".into());
    };
    let path = path.trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || owner.contains(' ') || repo.contains(' ') {
        return Err("could not read owner/repo from that URL".into());
    }
    let segment = parts.next().unwrap_or_default();
    if !segment.is_empty() {
        return Err("point at a repository root, not a subdirectory".into());
    }
    Ok((
        format!("https://github.com/{owner}/{repo}.git"),
        format!("{owner}/{repo}"),
    ))
}

fn git(
    args: &[&str],
    cwd: Option<&Path>,
    token: Option<&str>,
    url_override: Option<&str>,
) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        // Keep credentials out of argv; use an askpass-free header instead.
        command.env("GIT_CONFIG_COUNT", "1");
        command.env("GIT_CONFIG_KEY_0", "http.extraHeader");
        command.env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        );
    }
    let output = command
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    let _ = url_override;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "git failed".into()
        } else {
            crate::secrets::redact(&err)
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Clone a skills repo and return the skills it contains.
pub fn install(input: &str, token: Option<&str>) -> Result<Vec<SkillEntry>, String> {
    let (url, slug) = parse_source(input)?;
    let target = repos_dir().join(slug.replace('/', "__"));
    if target.join(".git").exists() {
        // Update an existing checkout instead of cloning twice.
        let _ = git(
            &["fetch", "--depth", "1", "origin"],
            Some(&target),
            token,
            None,
        );
        let _ = git(
            &["reset", "--hard", "FETCH_HEAD"],
            Some(&target),
            token,
            None,
        );
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create skills dir: {e}"))?;
        }
        git(
            &["clone", "--depth", "1", &url, &target.to_string_lossy()],
            None,
            token,
            None,
        )
        .map_err(|e| format!("clone failed: {e}"))?;
    }
    let skills = scan_repo(&slug, &target);
    if skills.is_empty() {
        return Err(format!(
            "{slug} has no SKILL.md files. Expected skills/<name>/SKILL.md."
        ));
    }
    Ok(skills)
}

pub fn remove_repo(slug: &str) -> Result<(), String> {
    let target = repos_dir().join(slug.replace('/', "__"));
    if !target.exists() {
        return Err(format!("{slug} is not installed"));
    }
    fs::remove_dir_all(&target).map_err(|e| format!("could not remove {slug}: {e}"))
}

/// All skills found in installed repos.
pub fn installed() -> Vec<SkillEntry> {
    let mut out = Vec::new();
    let dir = repos_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let repo = entry.file_name().to_string_lossy().replace("__", "/");
        out.extend(scan_repo(&repo, &entry.path()));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn scan_repo(repo: &str, root: &Path) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    collect(root, repo, 0, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn collect(dir: &Path, repo: &str, depth: usize, out: &mut Vec<SkillEntry>) {
    if depth > 4 || out.len() > 400 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if matches!(name.as_str(), ".git" | "node_modules" | "target") {
                continue;
            }
            collect(&path, repo, depth + 1, out);
        } else if name == "SKILL.md" {
            if let Some(skill) = parse_skill(repo, &path) {
                out.push(skill);
            }
        }
    }
}

fn parse_skill(repo: &str, path: &Path) -> Option<SkillEntry> {
    let text = fs::read_to_string(path).ok()?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "skill".into());
    let (front, _body) = split_frontmatter(&text);
    let mut name = dir_name.clone();
    let mut description = String::new();
    let mut allowed_tools = Vec::new();
    for (key, value) in front {
        match key.as_str() {
            "name" => {
                if !value.trim().is_empty() {
                    name = value.trim().to_string();
                }
            }
            "description" => description = value.trim().to_string(),
            "allowed-tools" | "allowed_tools" | "tools" => {
                allowed_tools = value
                    .trim_matches(|c| c == '[' || c == ']')
                    .split([',', ' '])
                    .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|item| !item.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    if description.is_empty() {
        description = first_paragraph(&text);
    }
    Some(SkillEntry {
        repo: repo.to_string(),
        name,
        description: clamp(&description, 400),
        allowed_tools,
        path: path.to_path_buf(),
    })
}

/// Skills committed in the project at `.hasty/skills/`, either as
/// `<name>/SKILL.md` or a flat `SKILL.md` set below that directory.
pub fn project_skills(root: &Path) -> Vec<SkillEntry> {
    let dir = root.join(".hasty").join("skills");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.join("SKILL.md").exists() {
                out.extend(scan_repo("project", &path));
            } else {
                collect(&path, "project", 0, &mut out);
            }
        } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
            if let Some(skill) = parse_skill("project", &path) {
                out.push(skill);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn split_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    let mut front = Vec::new();
    let mut body_start = 0usize;
    if let Some(rest) = text.strip_prefix("---") {
        let mut offset = 3;
        let mut last_key: Option<String> = None;
        let mut value_buffer = String::new();
        for line in rest.lines() {
            let line_len = line.len() + 1;
            if line.trim() == "---" {
                if let Some(key) = last_key.take() {
                    front.push((key, value_buffer.trim().to_string()));
                }
                body_start = offset + line_len;
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if !key.starts_with(' ') && !key.trim().is_empty() {
                    if let Some(prev) = last_key.take() {
                        front.push((prev, value_buffer.trim().to_string()));
                    }
                    value_buffer.clear();
                    last_key = Some(key.trim().to_string());
                    value_buffer.push_str(value.trim().trim_matches('"').trim_matches('\''));
                } else if last_key.is_some() {
                    value_buffer.push(' ');
                    value_buffer.push_str(line.trim());
                }
            }
            offset += line_len;
        }
    }
    (front, text.get(body_start..).unwrap_or("").to_string())
}

fn first_paragraph(text: &str) -> String {
    let body = split_frontmatter(text).1;
    let mut out = String::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !out.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    clamp(&out, 400)
}

/// Load a skill's instructions for the model. Bounded so one skill cannot
/// swallow the context window. `repo:name` disambiguates duplicates.
pub fn load(name: &str) -> Result<String, String> {
    load_in(None, name)
}

pub fn load_in(root: Option<&Path>, name: &str) -> Result<String, String> {
    let entry = find(name, root).ok_or_else(|| {
        format!("skill `{name}` is not installed. /skills lists available skills.")
    })?;
    let text = fs::read_to_string(&entry.path).map_err(|e| format!("cannot read skill: {e}"))?;
    let mut out = format!("# Skill: {}\nRepo: {}\n", entry.name, entry.repo);
    if !entry.allowed_tools.is_empty() {
        out.push_str(&format!(
            "Declared tools: {}\n",
            entry.allowed_tools.join(", ")
        ));
    }
    out.push('\n');
    out.push_str(&clamp(&text, 14_000));
    Ok(out)
}

fn find(name: &str, root: Option<&Path>) -> Option<SkillEntry> {
    let (repo_filter, bare) = match name.split_once(':') {
        Some((repo, skill)) => (Some(repo.to_string()), skill.to_string()),
        None => (None, name.to_string()),
    };
    let mut all = installed();
    if let Some(root) = root {
        all.extend(project_skills(root));
    }
    all.into_iter().find(|skill| {
        skill.name == bare
            && repo_filter
                .as_ref()
                .map(|repo| skill.repo == *repo)
                .unwrap_or(true)
    })
}

/// The compact index injected into the system prompt.
pub fn index(enabled: &[String], project_root: &Path) -> String {
    let all = installed();
    let mut out = String::new();
    let project_skills_dir = project_root.join(".hasty").join("skills");
    for skill in all
        .iter()
        .filter(|s| enabled.is_empty() || enabled.contains(&s.name))
    {
        out.push_str(&format!(
            "- {}: {} ({})\n",
            skill.name,
            clamp(&skill.description, 180),
            skill.repo
        ));
    }
    if project_skills_dir.exists() {
        for skill in project_skills(project_root) {
            out.push_str(&format!(
                "- project:{}: {}\n",
                skill.name,
                clamp(&skill.description, 180)
            ));
        }
    }
    out
}

fn clamp(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[..max].iter().collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_sources() {
        assert_eq!(
            parse_source("https://github.com/addyosmani/agent-skills").unwrap(),
            (
                "https://github.com/addyosmani/agent-skills.git".to_string(),
                "addyosmani/agent-skills".to_string()
            )
        );
        assert_eq!(
            parse_source("owner/repo").unwrap().1,
            "owner/repo".to_string()
        );
        assert!(parse_source("https://example.com/x/y").is_err());
    }

    #[test]
    fn parses_skill_frontmatter() {
        let text = "---\nname: test-driven-development\ndescription: Red, green, refactor.\n---\n\n# Skill\nBody";
        let (front, _) = split_frontmatter(text);
        assert_eq!(front[0].0, "name");
        assert_eq!(front[0].1, "test-driven-development");
        assert_eq!(front[1].1, "Red, green, refactor.");
    }

    #[test]
    fn defaults_suggestion_is_github() {
        assert!(DEFAULT_SUGGESTION.starts_with("https://github.com/"));
    }
}
