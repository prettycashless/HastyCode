//! Local SQLite storage. One database under `~/.hasty/hasty.db`.
//!
//! Tables: sessions, messages, tool_calls, subagent_runs, settings.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub summary: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub thinking: String,
    /// JSON array of tool calls made in this assistant message.
    #[serde(default)]
    pub tool_calls: String,
    /// For `role == "tool"`: the call this result answers.
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub name: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRow {
    pub id: i64,
    pub name: String,
    pub args: String,
    pub result: String,
    pub status: String,
    pub duration_ms: u64,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentRow {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub task: String,
    pub result: String,
    pub health: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            let _ = paths::restrict(path);
        }
        // The UI and the agent worker write through separate connections.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_default() -> rusqlite::Result<Self> {
        paths::ensure_home().ok();
        Self::open(paths::db_file())
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL DEFAULT '',
                cwd         TEXT NOT NULL DEFAULT '',
                provider    TEXT NOT NULL DEFAULT '',
                model       TEXT NOT NULL DEFAULT '',
                summary     TEXT NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL DEFAULT '',
                thinking    TEXT NOT NULL DEFAULT '',
                tool_calls  TEXT NOT NULL DEFAULT '',
                tool_call_id TEXT NOT NULL DEFAULT '',
                name        TEXT NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
            CREATE TABLE IF NOT EXISTS tool_calls (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                name        TEXT NOT NULL,
                args        TEXT NOT NULL DEFAULT '',
                result      TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'done',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                started_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls(session_id, id);
            CREATE TABLE IF NOT EXISTS subagent_runs (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                name        TEXT NOT NULL,
                provider    TEXT NOT NULL DEFAULT '',
                model       TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'queued',
                task        TEXT NOT NULL DEFAULT '',
                result      TEXT NOT NULL DEFAULT '',
                health      TEXT NOT NULL DEFAULT 'ok',
                started_at  INTEGER NOT NULL,
                ended_at    INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_subagent_session ON subagent_runs(session_id);
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        self.ensure_message_columns()
    }

    /// Bring databases created by older builds up to the current shape. New
    /// columns are added in place so existing sessions keep working.
    fn ensure_message_columns(&self) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(messages)")?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);
        for (name, ddl) in [
            (
                "tool_calls",
                "ALTER TABLE messages ADD COLUMN tool_calls TEXT NOT NULL DEFAULT ''",
            ),
            (
                "tool_call_id",
                "ALTER TABLE messages ADD COLUMN tool_call_id TEXT NOT NULL DEFAULT ''",
            ),
            (
                "name",
                "ALTER TABLE messages ADD COLUMN name TEXT NOT NULL DEFAULT ''",
            ),
        ] {
            if !existing.iter().any(|column| column == name) {
                self.conn.execute(ddl, [])?;
            }
        }
        self.conn.pragma_update(None, "user_version", 2)?;
        Ok(())
    }

    pub fn create_session(
        &self,
        title: &str,
        cwd: &str,
        provider: &str,
        model: &str,
    ) -> rusqlite::Result<Session> {
        let now = crate::config::now_secs();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO sessions (id, title, cwd, provider, model, summary, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, '', ?6, ?6)",
            params![id, title, cwd, provider, model, now],
        )?;
        Ok(Session {
            id,
            title: title.to_string(),
            cwd: cwd.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            summary: String::new(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn session(&self, id: &str) -> rusqlite::Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, title, cwd, provider, model, summary, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()
    }

    pub fn latest_session(&self) -> rusqlite::Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, title, cwd, provider, model, summary, created_at, updated_at
                 FROM sessions ORDER BY updated_at DESC LIMIT 1",
                [],
                row_to_session,
            )
            .optional()
    }

    pub fn list_sessions(&self, limit: usize) -> rusqlite::Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, cwd, provider, model, summary, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_session)?;
        rows.collect()
    }

    pub fn touch_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> rusqlite::Result<()> {
        let now = crate::config::now_secs();
        self.conn.execute(
            "UPDATE sessions SET updated_at = ?2,
                title = COALESCE(?3, title),
                model = COALESCE(?4, model)
             WHERE id = ?1",
            params![id, now, title, model],
        )?;
        Ok(())
    }

    pub fn set_summary(&self, id: &str, summary: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET summary = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, summary, crate::config::now_secs()],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> rusqlite::Result<usize> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
    }

    pub fn delete_all_sessions(&self) -> rusqlite::Result<usize> {
        self.conn.execute("DELETE FROM sessions", [])
    }

    pub fn add_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        thinking: &str,
    ) -> rusqlite::Result<i64> {
        self.add_message_full(session_id, role, content, thinking, "", "", "")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_message_full(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        thinking: &str,
        tool_calls: &str,
        tool_call_id: &str,
        name: &str,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO messages
               (session_id, role, content, thinking, tool_calls, tool_call_id, name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                role,
                content,
                thinking,
                tool_calls,
                tool_call_id,
                name,
                crate::config::now_secs()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Persist a tool result with the call id it answers, so a resumed session
    /// can rebuild a valid assistant/tool message pair.
    pub fn add_tool_result(
        &self,
        session_id: &str,
        call_id: &str,
        name: &str,
        content: &str,
    ) -> rusqlite::Result<i64> {
        self.add_message_full(session_id, "tool", content, "", "", call_id, name)
    }

    pub fn messages(&self, session_id: &str) -> rusqlite::Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, thinking, tool_calls, tool_call_id, name, created_at
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                thinking: row.get(3)?,
                tool_calls: row.get(4)?,
                tool_call_id: row.get(5)?,
                name: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    pub fn add_tool_call(
        &self,
        session_id: &str,
        name: &str,
        args: &str,
        result: &str,
        status: &str,
        duration_ms: u64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO tool_calls (session_id, name, args, result, status, duration_ms, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                name,
                args,
                result,
                status,
                duration_ms as i64,
                crate::config::now_secs()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn tool_calls(&self, session_id: &str) -> rusqlite::Result<Vec<ToolCallRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, args, result, status, duration_ms, started_at FROM tool_calls
             WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(ToolCallRow {
                id: row.get(0)?,
                name: row.get(1)?,
                args: row.get(2)?,
                result: row.get(3)?,
                status: row.get(4)?,
                duration_ms: row.get::<_, i64>(5)?.max(0) as u64,
                started_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_subagent(&self, row: &SubagentRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO subagent_runs
              (id, session_id, name, provider, model, status, task, result, health, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
               status = excluded.status,
               model = excluded.model,
               result = excluded.result,
               health = excluded.health,
               ended_at = excluded.ended_at",
            params![
                row.id,
                row.session_id,
                row.name,
                row.provider,
                row.model,
                row.status,
                row.task,
                row.result,
                row.health,
                row.started_at as i64,
                row.ended_at.map(|v| v as i64)
            ],
        )?;
        Ok(())
    }

    pub fn subagent_runs(&self, session_id: &str) -> rusqlite::Result<Vec<SubagentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, name, provider, model, status, task, result, health, started_at, ended_at
             FROM subagent_runs WHERE session_id = ?1 ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(SubagentRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                name: row.get(2)?,
                provider: row.get(3)?,
                model: row.get(4)?,
                status: row.get(5)?,
                task: row.get(6)?,
                result: row.get(7)?,
                health: row.get(8)?,
                started_at: row.get::<_, i64>(9)?.max(0) as u64,
                ended_at: row.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u64),
            })
        })?;
        rows.collect()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    }

    /// Full JSON export of one session, or every session.
    pub fn export_json(&self, session_id: Option<&str>) -> rusqlite::Result<String> {
        #[derive(Serialize)]
        struct Bundle {
            exported_at: u64,
            sessions: Vec<BundleSession>,
        }
        #[derive(Serialize)]
        struct BundleSession {
            session: Session,
            messages: Vec<MessageRow>,
            tool_calls: Vec<ToolCallRow>,
            subagents: Vec<SubagentRow>,
        }

        let sessions = match session_id {
            Some(id) => self.session(id)?.into_iter().collect::<Vec<_>>(),
            None => self.list_sessions(10_000)?,
        };
        let mut bundle = Bundle {
            exported_at: crate::config::now_secs(),
            sessions: Vec::new(),
        };
        for session in sessions {
            bundle.sessions.push(BundleSession {
                messages: self.messages(&session.id)?,
                tool_calls: self.tool_calls(&session.id)?,
                subagents: self.subagent_runs(&session.id)?,
                session,
            });
        }
        serde_json::to_string_pretty(&bundle).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
        })
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        cwd: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        summary: row.get(5)?,
        created_at: row.get::<_, i64>(6)?.max(0) as u64,
        updated_at: row.get::<_, i64>(7)?.max(0) as u64,
    })
}

/// Default location used by the CLI export command.
pub fn default_export_path() -> PathBuf {
    paths::home().join(format!("hasty-export-{}.json", crate::config::now_secs()))
}
