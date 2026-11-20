//! The single permission gate. Every tool action passes through
//! [`Gate::check`], which decides allow / confirm / deny.
//!
//! The catastrophic deny-list applies in *both* modes: "Full access" means no
//! prompts, not "destroy the machine without asking".

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    FullAccess,
    /// Confirm every command, install, app launch and out-of-project write.
    #[default]
    AskEveryTime,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::FullAccess => "Full access",
            Mode::AskEveryTime => "Ask every time",
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Mode::FullAccess => {
                "Runs tools without asking. Catastrophic commands are still blocked."
            }
            Mode::AskEveryTime => {
                "Asks before commands, installs, app launches and writes outside the project."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ReadFile { path: PathBuf },
    WriteFile { path: PathBuf },
    EditFile { path: PathBuf },
    DeleteFile { path: PathBuf },
    MoveFile { from: PathBuf, to: PathBuf },
    MakeDir { path: PathBuf },
    RunCommand { command: String },
    Install { what: String },
    OpenApp { app: String },
    Git { args: String },
    Network { url: String },
    SpawnSubagent { name: String },
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Action::ReadFile { path } => format!("read {}", path.display()),
            Action::WriteFile { path } => format!("write {}", path.display()),
            Action::EditFile { path } => format!("edit {}", path.display()),
            Action::DeleteFile { path } => format!("delete {}", path.display()),
            Action::MoveFile { from, to } => {
                format!("move {} to {}", from.display(), to.display())
            }
            Action::MakeDir { path } => format!("create {}", path.display()),
            Action::RunCommand { command } => format!("run `{}`", one_line(command, 120)),
            Action::Install { what } => format!("install {what}"),
            Action::OpenApp { app } => format!("open {app}"),
            Action::Git { args } => format!("git {args}"),
            Action::Network { url } => format!("fetch {url}"),
            Action::SpawnSubagent { name } => format!("spawn subagent {name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    /// Ask the user; the string is the reason shown in the prompt.
    Confirm(String),
    /// Refused outright — shown to the user and the model.
    Deny(String),
}

#[derive(Debug, Clone)]
pub struct Gate {
    pub mode: Mode,
    pub project_root: PathBuf,
    pub allow_outside_reads: bool,
}

impl Gate {
    pub fn new(mode: Mode, project_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            project_root: project_root.into(),
            allow_outside_reads: false,
        }
    }

    pub fn check(&self, action: &Action) -> Outcome {
        // 1. Catastrophic commands are blocked regardless of mode. The `git`
        // tool runs a shell command too, so it must be inspected the same way;
        // otherwise `git push --force` would never ask in ask mode.
        let git_command;
        let as_command = match action {
            Action::RunCommand { command } => Some(command.as_str()),
            Action::Git { args } => {
                git_command = format!("git {args}");
                Some(git_command.as_str())
            }
            _ => None,
        };
        if let Some(command) = as_command {
            if let Some(reason) = catastrophic(command) {
                return Outcome::Deny(reason);
            }
            // Destructive-but-recoverable commands are never a silent allow.
            if let Some(reason) = risky(command) {
                return Outcome::Confirm(reason);
            }
        }

        // 2. Sensitive locations always need confirmation, even in Full access.
        let paths = paths_of(action);
        for path in &paths {
            if let Some(reason) = sensitive_path(path) {
                if self.mode == Mode::FullAccess {
                    return Outcome::Confirm(format!(
                        "{reason}. This is outside normal project work."
                    ));
                }
                return Outcome::Confirm(reason);
            }
        }

        if self.mode == Mode::FullAccess {
            return Outcome::Allow;
        }

        // 3. Ask-every-time rules.
        match action {
            Action::RunCommand { .. } => Outcome::Confirm("Running a shell command".into()),
            Action::Install { .. } => Outcome::Confirm("Installing software".into()),
            Action::OpenApp { .. } => Outcome::Confirm("Launching an application".into()),
            Action::WriteFile { path }
            | Action::EditFile { path }
            | Action::DeleteFile { path } => {
                // Ask mode means ask. In-project edits show a diff and wait for
                // approval; outside the project they always confirm.
                if self.inside_project(path) {
                    Outcome::Confirm(format!("Changing {}", path.display()))
                } else {
                    Outcome::Confirm(format!("Writing outside the project: {}", path.display()))
                }
            }
            Action::MoveFile { from, to } => {
                Outcome::Confirm(format!("Moving {} to {}", from.display(), to.display()))
            }
            Action::MakeDir { path } => {
                if self.inside_project(path) {
                    Outcome::Allow
                } else {
                    Outcome::Confirm(format!(
                        "Creating a directory outside the project: {}",
                        path.display()
                    ))
                }
            }
            Action::ReadFile { path } => {
                if self.inside_project(path) || self.allow_outside_reads {
                    Outcome::Allow
                } else {
                    Outcome::Confirm(format!("Reading outside the project: {}", path.display()))
                }
            }
            Action::Git { .. } => Outcome::Confirm("Running git".into()),
            Action::Network { .. } => Outcome::Confirm("Network access".into()),
            Action::SpawnSubagent { .. } => Outcome::Allow,
        }
    }

    pub fn inside_project(&self, path: &Path) -> bool {
        let lexical_root = normalize(&self.project_root);
        let root = std::fs::canonicalize(&lexical_root).unwrap_or(lexical_root);
        let lexical = normalize(path);
        // Resolve symlinks so a link inside the project cannot point outside it.
        let target = match std::fs::canonicalize(&lexical) {
            Ok(real) => real,
            Err(_) => match lexical.parent().and_then(|p| std::fs::canonicalize(p).ok()) {
                Some(parent) => parent.join(lexical.file_name().unwrap_or_default()),
                None => lexical,
            },
        };
        target == root || target.starts_with(&root)
    }
}

fn paths_of(action: &Action) -> Vec<PathBuf> {
    match action {
        Action::ReadFile { path }
        | Action::WriteFile { path }
        | Action::EditFile { path }
        | Action::DeleteFile { path }
        | Action::MakeDir { path } => vec![path.clone()],
        Action::MoveFile { from, to } => vec![from.clone(), to.clone()],
        _ => Vec::new(),
    }
}

fn sensitive_path(path: &Path) -> Option<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    let lower = text.to_ascii_lowercase();
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
        .unwrap_or_default();
    let sensitive_dirs = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/gh",
        ".config/gcloud",
        ".kube",
        ".docker",
        ".password-store",
    ];
    for dir in sensitive_dirs {
        let needle = format!("/{dir}");
        if lower.contains(&needle) || lower.ends_with(dir) {
            return Some(format!("Touching credentials in {}", dir));
        }
    }
    // Secret-bearing files are just as sensitive as secret-bearing folders.
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let secret_files = [
        ".env",
        ".netrc",
        ".npmrc",
        ".pgpass",
        ".git-credentials",
        "credentials",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    let secret_suffix = [".pem", ".key", ".p12", ".pfx", ".keystore"];
    if secret_files.contains(&file_name.as_str())
        || file_name.starts_with(".env.")
        || secret_suffix
            .iter()
            .any(|suffix| file_name.ends_with(suffix))
    {
        return Some(format!("Reading a credential file ({file_name})"));
    }
    if !home.is_empty() {
        let vault = format!("{home}/.hasty");
        if lower.starts_with(&vault) {
            return Some("Touching Hasty's own config or key vault".into());
        }
    }
    None
}

/// Commands that are never acceptable, in any mode.
pub fn catastrophic(command: &str) -> Option<String> {
    let raw = command.trim();
    let c = collapse_ws(&raw.to_ascii_lowercase());

    let deny = |reason: &str| Some(reason.to_string());

    if c.contains("--no-preserve-root") {
        return deny("Blocked: --no-preserve-root");
    }
    if let Some(reason) = recursive_delete_of_root(&c) {
        return deny(&reason);
    }

    if c.contains("mkfs") || c.contains("mke2fs") {
        return deny("Blocked: formatting a filesystem");
    }
    if c.contains("dd ") && c.contains("of=/dev/") {
        return deny("Blocked: raw write to a block device");
    }
    if c.contains("> /dev/sd")
        || c.contains("> /dev/nvme")
        || c.contains("> /dev/disk")
        || c.contains("of=/dev/sd")
        || c.contains("of=/dev/nvme")
        || c.contains("of=/dev/disk")
    {
        return deny("Blocked: writing directly to a disk device");
    }
    if c.contains(":(){ :|:& };:") || c.contains(":(){:|:&};:") {
        return deny("Blocked: fork bomb");
    }
    if c.contains("chmod -r 777 /") || c.contains("chmod -r 000 /") {
        return deny("Blocked: recursive permission change on root");
    }
    if c.contains("shred /dev/") {
        return deny("Blocked: shredding a device");
    }
    if (c.contains("curl ") || c.contains("wget ")) && c.contains("| sh") && c.contains("sudo") {
        return deny("Blocked: piping a remote script into a privileged shell");
    }
    if c.contains("sudo rm -rf /") {
        return deny("Blocked: privileged recursive delete of root");
    }
    if c.contains("diskpart") && c.contains("clean") {
        return deny("Blocked: diskpart clean");
    }
    if c.contains("format c:") || c.contains("format d:") {
        return deny("Blocked: formatting a drive");
    }
    if let Some(reason) = destructive_find(&c) {
        return deny(&reason);
    }
    None
}

/// Destructive but recoverable commands. These must never be a silent allow,
/// but they are legitimate sometimes, so they confirm instead of denying.
pub fn risky(command: &str) -> Option<String> {
    let c = collapse_ws(&command.trim().to_ascii_lowercase());
    if c.contains("git push")
        && (c.contains(" --force")
            || c.contains(" -f ")
            || c.contains("--force-with-lease")
            || c.contains(" -f"))
    {
        return Some("Force-push rewrites shared history.".into());
    }
    if c.contains("git reset --hard") {
        return Some("This discards uncommitted work.".into());
    }
    if c.contains("git clean") && (c.contains(" -fd") || c.contains(" -xfd") || c.contains(" -f")) {
        return Some("This deletes untracked files.".into());
    }
    if c.contains("git checkout -- .") || c.contains("git restore .") {
        return Some("This discards uncommitted changes.".into());
    }
    None
}

/// `find / -delete`, `find ~ -exec rm ...` and friends.
fn destructive_find(command: &str) -> Option<String> {
    for segment in command.split([';', '|', '&', '\n']) {
        let tokens = shell_words(segment);
        let Some(find_at) = tokens.iter().position(|token| {
            let t = unquote(token);
            t == "find" || t.ends_with("/find")
        }) else {
            continue;
        };
        let targets_root = tokens[find_at + 1..].iter().any(|token| {
            let raw = unquote(token);
            // Trim trailing slashes, but keep a bare `/` recognizable.
            let t = if raw == "/" {
                "/"
            } else {
                raw.trim_end_matches('/')
            };
            matches!(t, "/" | "*" | "/*" | "~" | "$home" | "${home}" | ".")
        });
        let deletes = tokens[find_at + 1..].iter().any(|token| {
            let t = unquote(token);
            t == "-delete" || t == "-exec" || t == "-execdir"
        });
        if targets_root && deletes {
            return Some("Blocked: find deleting from the root of the filesystem or home".into());
        }
    }
    None
}

fn unquote(token: &str) -> &str {
    token.trim_matches('"').trim_matches('\'')
}

/// Split a shell fragment into words for detection. Grouping, command
/// substitution and quoting become separators, so `$(rm -rf /)` is inspected
/// as the `rm -rf /` it really is.
fn shell_words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for ch in segment.chars() {
        if ch.is_whitespace() || matches!(ch, '(' | ')' | '`' | '"' | '\'') {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out.retain(|token| token != "$");
    out
}

/// Detect `rm -r`-style deletion aimed at `/`, `~`, `$HOME` or a bare glob.
/// Tokenized so `rm -rf /tmp/build` stays allowed.
fn recursive_delete_of_root(command: &str) -> Option<String> {
    for segment in command.split([';', '|', '&', '\n']) {
        let tokens = shell_words(segment);
        let Some(rm_at) = tokens.iter().position(|token| {
            let t = unquote(token);
            t == "rm" || t.ends_with("/rm")
        }) else {
            continue;
        };
        let recursive = tokens[rm_at + 1..].iter().any(|token| {
            (token.starts_with('-') && !token.starts_with("--") && token.contains('r'))
                || *token == "--recursive"
        });
        if !recursive {
            continue;
        }
        for target in tokens[rm_at + 1..]
            .iter()
            .filter(|token| !token.starts_with('-'))
        {
            let raw = unquote(target).trim_end_matches('/');
            let target = if raw.is_empty() { "/" } else { raw };
            if matches!(target, "/" | "*" | "/*" | "~" | "$home" | "${home}" | ".") {
                return Some(format!("Blocked: recursive delete of `{target}`"));
            }
        }
    }
    None
}

fn normalize(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    // Resolve `..` lexically; symlinks are not followed (cheap and predictable).
    let mut out = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(mode: Mode) -> Gate {
        Gate::new(mode, "/work/project")
    }

    #[test]
    fn full_access_allows_commands() {
        assert_eq!(
            gate(Mode::FullAccess).check(&Action::RunCommand {
                command: "cargo test".into()
            }),
            Outcome::Allow
        );
    }

    #[test]
    fn ask_mode_confirms_every_command() {
        let outcome = gate(Mode::AskEveryTime).check(&Action::RunCommand {
            command: "ls".into(),
        });
        assert!(matches!(outcome, Outcome::Confirm(_)));
    }

    #[test]
    fn in_project_edits_confirm_in_ask_mode() {
        assert!(matches!(
            gate(Mode::AskEveryTime).check(&Action::WriteFile {
                path: PathBuf::from("/work/project/src/main.rs")
            }),
            Outcome::Confirm(_)
        ));
    }

    #[test]
    fn deny_list_sees_through_shell_wrappers() {
        for cmd in [
            "sh -c 'rm -rf /'",
            "bash -c \"rm -rf ~\"",
            "find / -delete",
            "find ~ -exec rm {} +",
        ] {
            assert!(catastrophic(cmd).is_some(), "{cmd} should be denied");
        }
    }

    #[test]
    fn credential_files_confirm_even_in_full_access() {
        let g = gate(Mode::FullAccess);
        for path in [
            "/work/project/.env",
            "/work/project/.env.local",
            "/work/project/deploy.pem",
            "/work/project/id_rsa",
        ] {
            assert!(
                matches!(
                    g.check(&Action::ReadFile {
                        path: PathBuf::from(path)
                    }),
                    Outcome::Confirm(_)
                ),
                "{path} should confirm"
            );
        }
    }

    #[test]
    fn destructive_git_confirms_instead_of_denying() {
        assert!(matches!(
            gate(Mode::FullAccess).check(&Action::RunCommand {
                command: "git reset --hard HEAD~1".into()
            }),
            Outcome::Confirm(_)
        ));
    }

    #[test]
    fn git_tool_is_gated_like_a_command() {
        // The git tool used to be a silent allow, so a force-push never asked.
        assert!(matches!(
            gate(Mode::FullAccess).check(&Action::Git {
                args: "push --force origin main".into()
            }),
            Outcome::Confirm(_)
        ));
        assert!(matches!(
            gate(Mode::AskEveryTime).check(&Action::Git {
                args: "status".into()
            }),
            Outcome::Confirm(_)
        ));
        assert_eq!(
            gate(Mode::FullAccess).check(&Action::Git {
                args: "status".into()
            }),
            Outcome::Allow
        );
    }

    #[test]
    fn command_substitution_does_not_hide_a_wipe() {
        for cmd in [
            "echo $(rm -rf /)",
            "echo `rm -rf /`",
            "echo $(find / -delete)",
        ] {
            assert!(catastrophic(cmd).is_some(), "{cmd} should be denied");
        }
    }

    #[test]
    fn writes_outside_project_confirm_in_ask_mode() {
        assert!(matches!(
            gate(Mode::AskEveryTime).check(&Action::WriteFile {
                path: PathBuf::from("/etc/passwd")
            }),
            Outcome::Confirm(_)
        ));
    }

    #[test]
    fn destructive_commands_are_denied_in_full_access() {
        for cmd in [
            "rm -rf /",
            "sudo rm -rf / --no-preserve-root",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            ":(){ :|:& };:",
        ] {
            let outcome = gate(Mode::FullAccess).check(&Action::RunCommand {
                command: cmd.into(),
            });
            assert!(
                matches!(outcome, Outcome::Deny(_)),
                "{cmd} should be denied, got {outcome:?}"
            );
        }
    }

    #[test]
    fn credential_paths_need_confirmation_even_in_full_access() {
        let home = dirs::home_dir().unwrap();
        let outcome = gate(Mode::FullAccess).check(&Action::ReadFile {
            path: home.join(".ssh/id_rsa"),
        });
        assert!(matches!(outcome, Outcome::Confirm(_)));
    }

    #[test]
    fn destructive_git_is_not_a_hard_deny() {
        assert!(risky("git push --force origin main").is_some());
        assert!(risky("git reset --hard HEAD~3").is_some());
        assert!(catastrophic("git reset --hard HEAD~3").is_none());
    }

    #[test]
    fn path_escape_is_normalized() {
        let g = gate(Mode::AskEveryTime);
        assert!(!g.inside_project(Path::new("/work/project/../../etc/passwd")));
        assert!(g.inside_project(Path::new("/work/project/./src/lib.rs")));
    }
}
