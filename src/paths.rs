//! Local filesystem locations used by Hasty.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Root directory. `HASTY_HOME` overrides it (useful for tests and VMs).
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("HASTY_HOME") {
        return PathBuf::from(dir);
    }
    match dirs::home_dir() {
        Some(home) => home.join(".hasty"),
        None => PathBuf::from(".hasty"),
    }
}

pub fn config_file() -> PathBuf {
    home().join("config.json")
}

pub fn db_file() -> PathBuf {
    home().join("hasty.db")
}

pub fn vault_file() -> PathBuf {
    home().join("vault.json")
}

pub fn personality_file() -> PathBuf {
    home().join("personality.md")
}

pub fn skills_dir() -> PathBuf {
    home().join("skills")
}

pub fn logs_dir() -> PathBuf {
    home().join("logs")
}

/// Create the Hasty home directory with owner-only permissions where supported.
pub fn ensure_home() -> io::Result<PathBuf> {
    let dir = home();
    fs::create_dir_all(&dir)?;
    restrict(&dir)?;
    Ok(dir)
}

/// Best-effort: strip group/other access on Unix. No-op on Windows.
pub fn restrict(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path)?;
        let mut perms = meta.permissions();
        let mode = perms.mode();
        let want = if meta.is_dir() { 0o700 } else { 0o600 };
        if mode & 0o777 != want {
            perms.set_mode(want);
            fs::set_permissions(path, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Write a file atomically and with restrictive permissions.
pub fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, contents)?;
    restrict(&tmp)?;
    fs::rename(&tmp, path)?;
    restrict(path)?;
    Ok(())
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}
