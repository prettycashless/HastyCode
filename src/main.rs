use std::process::ExitCode;

use hasty::commands;
use hasty::config::Config;
use hasty::db::Store;
use hasty::paths;
use hasty::personality;
use hasty::providers;
use hasty::secrets::Secrets;
use hasty::tui::{self, App};
use hasty::wizard;
use hasty::VERSION;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("hasty: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(|s| s.as_str()).unwrap_or("");

    match first {
        "--version" | "-V" | "version" => {
            println!("hasty {VERSION}");
            return Ok(ExitCode::SUCCESS);
        }
        "--help" | "-h" | "help" => {
            print_help();
            return Ok(ExitCode::SUCCESS);
        }
        "export" => return export(&args[1..]),
        "sessions" => return list_sessions(),
        "delete" => return delete(&args[1..]),
        "setup" => {}
        "" => {}
        other => {
            eprintln!("hasty: unknown argument `{other}`. Try `hasty --help`.");
            return Ok(ExitCode::FAILURE);
        }
    }

    paths::ensure_home()?;
    personality::ensure_file()?;
    let cfg = Config::load()?;
    let preferred = if cfg.secret_backend.is_empty() {
        None
    } else {
        Some(cfg.secret_backend.as_str())
    };
    let secrets = Secrets::open(preferred).map_err(ioerr)?;
    let store = Store::open_default()?;

    let force_setup = first == "setup";
    let (cfg, secrets) = if !cfg.onboarded || force_setup {
        match wizard::run(cfg, secrets)? {
            Some(pair) => pair,
            None => {
                println!("Setup cancelled. Nothing was changed.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    } else {
        (cfg, secrets)
    };
    drop(store);

    run_tui(cfg, secrets)
}

fn ioerr(message: String) -> std::io::Error {
    std::io::Error::other(message)
}

fn run_tui(cfg: Config, secrets: Secrets) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let root = std::env::current_dir()?;
    let cwd = root.to_string_lossy().to_string();
    let session = store.create_session("", &cwd, &cfg.provider, &cfg.model)?;
    let session_id = session.id.clone();
    let app = App::new(
        cfg,
        secrets,
        store,
        root,
        session.id,
        Vec::new(),
        String::new(),
    )?;
    tui::run(app)?;
    // Opening Hasty and leaving without sending anything should not leave an
    // empty session behind.
    if let Ok(check) = Store::open_default() {
        if check
            .messages(&session_id)
            .map(|messages| messages.is_empty())
            .unwrap_or(false)
        {
            let _ = check.delete_session(&session_id);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn export(args: &[String]) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let mut path: Option<std::path::PathBuf> = None;
    let mut session: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session" | "-s" => {
                index += 1;
                session = args.get(index).cloned();
            }
            other => path = Some(std::path::PathBuf::from(other)),
        }
        index += 1;
    }
    let written =
        commands::export_session(&store, path.as_deref(), session.as_deref()).map_err(ioerr)?;
    println!("Exported to {}", written.display());
    Ok(ExitCode::SUCCESS)
}

fn list_sessions() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let sessions = store.list_sessions(100)?;
    if sessions.is_empty() {
        println!("No sessions yet.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<38}  {:<30}  {:<24}  messages", "id", "title", "model");
    for session in sessions {
        let count = store.messages(&session.id).map(|m| m.len()).unwrap_or(0);
        let title = if session.title.is_empty() {
            "(untitled)".to_string()
        } else {
            session.title.chars().take(30).collect()
        };
        println!(
            "{:<38}  {:<30}  {:<24}  {}",
            session.id,
            title,
            format!("{}:{}", session.provider, session.model),
            count
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn delete(args: &[String]) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let Some(target) = args.first() else {
        eprintln!("Usage: hasty delete <session-id|all>");
        return Ok(ExitCode::FAILURE);
    };
    let store = Store::open_default()?;
    if target == "all" {
        let removed = store.delete_all_sessions()?;
        println!("Deleted {removed} session(s) and their messages.");
    } else if store.session(target)?.is_none() {
        eprintln!("No session with id {target}.");
        return Ok(ExitCode::FAILURE);
    } else {
        store.delete_session(target)?;
        println!("Deleted session {target}.");
    }
    Ok(ExitCode::SUCCESS)
}

fn print_help() {
    println!(
        "hasty {VERSION} — a terminal-only AI coding agent

USAGE
  hasty                     start the TUI (runs setup on first launch)
  hasty setup               re-run the setup wizard
  hasty sessions            list saved sessions
  hasty export [file] [--session <id>]
                            export session data as JSON
  hasty delete <id|all>     delete a session and its messages
  hasty --version           print the version
  hasty --help              this help

FILES
  {}  settings
  {}  sessions and messages (SQLite)
  {}  encrypted key vault
  {}  her personality

Providers: {}
",
        paths::config_file().display(),
        paths::db_file().display(),
        paths::vault_file().display(),
        personality::path().display(),
        providers::PROVIDERS
            .iter()
            .map(|(_, label)| *label)
            .collect::<Vec<_>>()
            .join(", ")
    );
}
