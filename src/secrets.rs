//! Secret storage.
//!
//! Keys live either in the OS keychain (macOS `security`, Linux
//! `secret-tool`) or in an AES-256-GCM encrypted file with `0600` permissions.
//! The encryption key is derived from a machine identifier plus a random salt,
//! so copying the vault to another machine does not reveal it. Values are never
//! logged or printed.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    File,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Keychain => "keychain",
            Backend::File => "file",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    salt: String,
    nonce: String,
    cipher: String,
}

pub struct Secrets {
    backend: Backend,
    path: PathBuf,
    key: [u8; 32],
    salt: [u8; 16],
    map: BTreeMap<String, String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("backend", &self.backend)
            .field("path", &self.path)
            .field("keys", &self.map.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    /// Open (or create) the local vault. `preferred` may be "keychain" or
    /// "file"; an unavailable keychain falls back to the encrypted file.
    pub fn open(preferred: Option<&str>) -> Result<Self, String> {
        let path = paths::vault_file();
        let (salt, map) = if path.exists() {
            let (salt, payload) = read_vault_parts(&path)?;
            (salt, payload)
        } else {
            let mut salt = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut salt);
            (salt, None)
        };
        let key = derive_key(&machine_id(), &salt);
        let map = match map {
            Some(cipher) => decrypt(&path, &key, cipher)?,
            None => BTreeMap::new(),
        };

        let backend = match preferred {
            Some("file") => Backend::File,
            _ if keychain_available() => Backend::Keychain,
            _ => Backend::File,
        };

        let secrets = Self {
            backend,
            path,
            key,
            salt,
            map,
        };
        if !secrets.path.exists() {
            secrets.save()?;
        }
        Ok(secrets)
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn get(&self, name: &str) -> Option<String> {
        if self.backend == Backend::Keychain {
            if let Some(value) = keychain_get(name) {
                return Some(value);
            }
        }
        self.map.get(name).cloned()
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("empty secret".into());
        }
        self.map.insert(name.to_string(), value.to_string());
        // Always keep an encrypted local copy so the agent works offline and
        // after a keychain lock-out.
        self.save()?;
        if self.backend == Backend::Keychain && !keychain_set(name, value) {
            // Keychain refused; stay functional on the encrypted file.
            self.backend = Backend::File;
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        self.map.remove(name);
        self.save()?;
        if self.backend == Backend::Keychain {
            keychain_delete(name);
        }
        Ok(())
    }

    pub fn names(&self) -> Vec<&str> {
        self.map.keys().map(|s| s.as_str()).collect()
    }

    fn save(&self) -> Result<(), String> {
        let plain = serde_json::to_vec(&self.map).map_err(|e| e.to_string())?;
        let mut nonce_bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| e.to_string())?
            .encrypt(nonce, plain.as_ref())
            .map_err(|_| "failed to encrypt vault".to_string())?;
        let file = VaultFile {
            version: 1,
            salt: b64(self.salt.as_slice()),
            nonce: b64(&nonce_bytes),
            cipher: b64(&cipher),
        };
        let raw = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        paths::write_private(&self.path, &raw).map_err(|e| e.to_string())
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn read_vault_parts(path: &Path) -> Result<([u8; 16], Option<String>), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: VaultFile =
        serde_json::from_str(&raw).map_err(|_| "vault is unreadable".to_string())?;
    let salt_bytes = base64::engine::general_purpose::STANDARD
        .decode(file.salt)
        .map_err(|_| "vault salt is corrupt".to_string())?;
    if salt_bytes.len() != 16 {
        return Err("vault salt is corrupt".into());
    }
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&salt_bytes);
    Ok((salt, Some(file.cipher)))
}

fn decrypt(
    path: &Path,
    key: &[u8; 32],
    cipher_b64: String,
) -> Result<BTreeMap<String, String>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: VaultFile =
        serde_json::from_str(&raw).map_err(|_| "vault is unreadable".to_string())?;
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(file.nonce)
        .map_err(|_| "vault nonce is corrupt".to_string())?;
    let cipher = base64::engine::general_purpose::STANDARD
        .decode(cipher_b64)
        .map_err(|_| "vault data is corrupt".to_string())?;
    if nonce.len() != 12 {
        return Err("vault nonce is corrupt".into());
    }
    let plain = Aes256Gcm::new_from_slice(key)
        .map_err(|e| e.to_string())?
        .decrypt(Nonce::from_slice(&nonce), cipher.as_ref())
        .map_err(|_| {
            "vault could not be decrypted on this machine (wrong machine or vault moved)"
                .to_string()
        })?;
    serde_json::from_slice(&plain).map_err(|e| e.to_string())
}

fn derive_key(machine: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"hasty-vault-v1");
    h.update(machine.as_bytes());
    h.update(salt);
    let digest = h.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

fn machine_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(id) = std::fs::read_to_string(path) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
    }
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".into());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown-user".into());
    format!("{host}:{user}")
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn keychain_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        which("security")
    }
    #[cfg(target_os = "linux")]
    {
        which("secret-tool")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn keychain_get(name: &str) -> Option<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "hasty", "-a", name, "-w"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain_get(name: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("secret-tool")
            .args(["lookup", "service", "hasty", "name", name])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        None
    }
}

#[cfg(target_os = "macos")]
fn keychain_set(name: &str, value: &str) -> bool {
    // `security` has no stdin form for the password on macOS.
    Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            "hasty",
            "-a",
            name,
            "-w",
            value,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn keychain_set(name: &str, value: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        let mut child = match Command::new("secret-tool")
            .args([
                "store",
                "--label=Hasty API key",
                "service",
                "hasty",
                "name",
                name,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(value.as_bytes()).is_err() {
                return false;
            }
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, value);
        false
    }
}

#[cfg(target_os = "macos")]
fn keychain_delete(name: &str) {
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", "hasty", "-a", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(target_os = "macos"))]
fn keychain_delete(name: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("secret-tool")
            .args(["clear", "service", "hasty", "name", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
    }
}

/// Mask a key for display: keep at most the last four characters.
pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// Remove anything that looks like a key from text before it is rendered or logged.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim();
        let has_key_prefix = trimmed.starts_with("sk-")
            || trimmed.starts_with("sk_")
            || trimmed.starts_with("ghp_")
            || trimmed.starts_with("gho_")
            || trimmed.starts_with("github_pat_")
            || trimmed.starts_with("glpat-")
            || trimmed.starts_with("xoxb-")
            || trimmed.starts_with("xoxp-")
            || trimmed.starts_with("AKIA")
            || trimmed.starts_with("AIza")
            || trimmed.starts_with("hf_")
            || trimmed.starts_with("npm_")
            || trimmed.starts_with("xai-")
            || trimmed.starts_with("-----BEGIN");
        let looks_secret = (has_key_prefix && trimmed.len() > 12) || looks_like_jwt(trimmed);
        if looks_secret {
            out.push_str(&mask(trimmed));
            if token.ends_with(char::is_whitespace) {
                out.push(' ');
            }
        } else {
            out.push_str(token);
        }
    }
    out
}

/// Compact JWTs (`header.payload.signature`) are bearer tokens in practice.
fn looks_like_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3 && parts[0].starts_with("eyJ") && parts[1].len() > 10 && parts[2].len() > 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_short_and_long_values() {
        assert_eq!(mask("short"), "••••••••");
        assert_eq!(mask("sk-abcdefgh1234"), "••••1234");
    }

    #[test]
    fn redacts_key_like_tokens() {
        let out = redact("use sk-abcdefghijklmnop now");
        assert!(!out.contains("abcdefghijkl"));
        assert!(out.contains("use"));
    }

    #[test]
    fn redacts_cloud_and_scm_tokens() {
        for value in [
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwx",
            "xoxb-1234567890-abcdefghij",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ] {
            let out = redact(&format!("token {value} end"));
            assert!(!out.contains(value), "{value} was not redacted from {out}");
        }
    }
}
