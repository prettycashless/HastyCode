//! SQLite layer: sessions, messages, tool calls, subagent runs, export, delete.

use hasty::db::{Store, SubagentRow};

fn temp_store() -> (Store, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("hasty-test-{}.db", uuid::Uuid::new_v4()));
    let store = Store::open(&path).expect("open database");
    (store, path)
}

#[test]
fn sessions_round_trip_and_update() {
    let (store, path) = temp_store();
    let session = store
        .create_session("Fix build", "/tmp/project", "openai", "gpt-5")
        .unwrap();
    assert!(session.id.len() > 8);

    store
        .touch_session(&session.id, Some("Rename"), Some("gpt-5-mini"))
        .unwrap();
    let loaded = store.session(&session.id).unwrap().unwrap();
    assert_eq!(loaded.title, "Rename");
    assert_eq!(loaded.model, "gpt-5-mini");
    assert_eq!(loaded.provider, "openai");

    let listed = store.list_sessions(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, session.id);

    let latest = store.latest_session().unwrap().unwrap();
    assert_eq!(latest.id, session.id);

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn messages_and_tool_calls_persist_in_order() {
    let (store, path) = temp_store();
    let session = store
        .create_session("", "/tmp", "anthropic", "claude")
        .unwrap();
    store.add_message(&session.id, "user", "hello", "").unwrap();
    store
        .add_message(&session.id, "assistant", "working", "let me look")
        .unwrap();
    store
        .add_tool_call(
            &session.id,
            "read_file",
            r#"{"path":"a"}"#,
            "contents",
            "ok",
            12,
        )
        .unwrap();

    let messages = store.messages(&session.id).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].thinking, "let me look");

    let calls = store.tool_calls(&session.id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].duration_ms, 12);

    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.messages(&session.id).unwrap().len(), 2);
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[test]
fn deleting_a_session_cascades() {
    let (store, path) = temp_store();
    let session = store.create_session("", "/tmp", "openai", "gpt-5").unwrap();
    store.add_message(&session.id, "user", "hi", "").unwrap();
    store
        .add_tool_call(&session.id, "glob", "{}", "", "ok", 1)
        .unwrap();

    assert_eq!(store.delete_session(&session.id).unwrap(), 1);
    assert!(store.session(&session.id).unwrap().is_none());
    assert!(store.messages(&session.id).unwrap().is_empty());
    assert!(store.tool_calls(&session.id).unwrap().is_empty());

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn subagent_runs_are_upserted_and_exported() {
    let (store, path) = temp_store();
    let session = store.create_session("", "/tmp", "openai", "gpt-5").unwrap();
    let mut row = SubagentRow {
        id: "run-1".into(),
        session_id: session.id.clone(),
        name: "reviewer".into(),
        provider: "openai".into(),
        model: "gpt-5-mini".into(),
        status: "running".into(),
        task: "review the diff".into(),
        result: String::new(),
        health: "ok".into(),
        started_at: 10,
        ended_at: None,
    };
    store.upsert_subagent(&row).unwrap();
    row.status = "done".into();
    row.model = "gpt-5-mini-fallback".into();
    row.result = "looks fine".into();
    row.ended_at = Some(20);
    store.upsert_subagent(&row).unwrap();

    let runs = store.subagent_runs(&session.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "done");
    assert_eq!(runs[0].model, "gpt-5-mini-fallback");
    assert_eq!(runs[0].ended_at, Some(20));

    let export = store.export_json(Some(&session.id)).unwrap();
    assert!(export.contains("reviewer"));
    assert!(export.contains("looks fine"));
    assert!(export.contains(&session.id));

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn tool_calls_and_results_survive_a_resume() {
    let (store, path) = temp_store();
    let session = store.create_session("", "/tmp", "openai", "gpt-5").unwrap();
    store
        .add_message(&session.id, "user", "read it", "")
        .unwrap();
    let call = hasty::providers::ToolCall {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path": "a"}),
    };
    let calls_json = serde_json::to_string(&vec![call]).unwrap();
    store
        .add_message_full(&session.id, "assistant", "reading", "", &calls_json, "", "")
        .unwrap();
    store
        .add_tool_result(&session.id, "call_1", "read_file", "hi")
        .unwrap();

    let rows = store.messages(&session.id).unwrap();
    assert_eq!(rows[1].tool_calls, calls_json);
    assert_eq!(rows[2].tool_call_id, "call_1");

    let messages = hasty::agent::messages_from_rows(&rows);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[1].tool_calls[0].name, "read_file");
    assert_eq!(messages[2].role, hasty::providers::Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn older_databases_gain_the_new_message_columns() {
    let path = std::env::temp_dir().join(format!("hasty-old-{}.db", uuid::Uuid::new_v4()));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                thinking TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let session = store.create_session("", "/tmp", "openai", "gpt-5").unwrap();
    store
        .add_tool_result(&session.id, "old_call", "glob", "files")
        .unwrap();
    let rows = store.messages(&session.id).unwrap();
    assert_eq!(rows[0].tool_call_id, "old_call");
    assert_eq!(rows[0].name, "glob");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn clearing_a_session_is_durable() {
    let (store, path) = temp_store();
    let session = store
        .create_session("t", "/tmp", "openai", "gpt-5")
        .unwrap();
    store.add_message(&session.id, "user", "hello", "").unwrap();
    store.set_summary(&session.id, "a summary").unwrap();
    let removed = store.clear_messages(&session.id).unwrap();
    assert_eq!(removed, 1);
    assert!(store.messages(&session.id).unwrap().is_empty());
    let reloaded = store.session(&session.id).unwrap().unwrap();
    assert_eq!(reloaded.summary, "");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn latest_session_is_scoped_to_its_directory() {
    let (store, path) = temp_store();
    let first = store
        .create_session("a", "/proj", "openai", "gpt-5")
        .unwrap();
    let second = store
        .create_session("b", "/proj", "openai", "gpt-5")
        .unwrap();
    store
        .create_session("c", "/other", "openai", "gpt-5")
        .unwrap();
    let latest = store.latest_session_for_cwd("/proj").unwrap().unwrap();
    assert_eq!(latest.id, second.id);
    assert_ne!(latest.id, first.id);
    assert!(store.latest_session_for_cwd("/missing").unwrap().is_none());
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn usage_accumulates_on_the_session() {
    let (store, path) = temp_store();
    let session = store
        .create_session("t", "/tmp", "openai", "gpt-5")
        .unwrap();
    store.add_usage(&session.id, 10, 5).unwrap();
    store.add_usage(&session.id, 3, 2).unwrap();
    let reloaded = store.session(&session.id).unwrap().unwrap();
    assert_eq!((reloaded.input_tokens, reloaded.output_tokens), (13, 7));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn settings_are_stored_and_replaceable() {
    let (store, path) = temp_store();
    store.set_setting("theme", "dark-red").unwrap();
    store.set_setting("theme", "ember").unwrap();
    assert_eq!(store.get_setting("theme").unwrap(), Some("ember".into()));
    assert_eq!(store.get_setting("missing").unwrap(), None);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn export_all_sessions_is_valid_json() {
    let (store, path) = temp_store();
    let a = store
        .create_session("one", "/tmp", "openai", "gpt-5")
        .unwrap();
    let b = store
        .create_session("two", "/tmp", "google", "gemini")
        .unwrap();
    store.add_message(&a.id, "user", "first", "").unwrap();
    store.add_message(&b.id, "user", "second", "").unwrap();

    let export = store.export_json(None).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&export).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    drop(store);
    let _ = std::fs::remove_file(path);
}
