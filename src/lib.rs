//! Hasty — a local, terminal-only AI coding agent.
//!
//! Everything runs on the user's machine. There is no cloud backend, no
//! telemetry, and no key leaves the local process except in an authenticated
//! request to the provider the user configured.

pub mod agent;
pub mod commands;
pub mod config;
pub mod db;
pub mod memory;
pub mod paths;
pub mod permissions;
pub mod personality;
pub mod plain;
pub mod providers;
pub mod secrets;
pub mod skills;
pub mod theme;
pub mod tools;
pub mod tui;
pub mod wizard;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The ASCII wordmark shown on startup.
pub const LOGO: &str = r#"
  ██╗  ██╗ █████╗ ███████╗████████╗██╗   ██╗
  ██║  ██║██╔══██╗██╔════╝╚══██╔══╝╚██╗ ██╔╝
  ███████║███████║███████╗   ██║    ╚████╔╝
  ██╔══██║██╔══██║╚════██║   ██║     ╚██╔╝
  ██║  ██║██║  ██║███████║   ██║      ██║
  ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝   ╚═╝      ╚═╝
"#;
