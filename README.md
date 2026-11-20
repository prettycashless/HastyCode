# Hasty

A terminal-only AI coding agent. Local SQLite storage, local keys, no telemetry,
no cloud backend. Built in Rust with [ratatui](https://ratatui.rs).

Hasty is female, serious and dry. She plans, calls tools, checks the result and
re-plans when something fails. She will tell you when a request is a bad idea.

> ## ⚠️ Warning
>
> **Hasty can read, modify and delete anything on your machine.** It can run any
> shell command you can, install software, open applications, rewrite files and
> push git history. There is no sandbox.
>
> - Use **Ask every time** mode (the default) until you trust a workflow.
> - Run Hasty in a **VM, container, or disposable checkout** for untrusted
>   projects. A prompt injection in a README or web page can otherwise direct a
>   command at your real files.
> - Full access mode never asks. A hard deny-list blocks catastrophic commands
>   (`rm -rf /`, `mkfs`, raw writes to disks, fork bombs), but it is a safety net,
>   not a sandbox.
> - Review what she runs. You are responsible for the commands you approve.

---

## Install

Requires Rust 1.78+ (build from source; a static binary is produced).

```bash
git clone <this repo> hasty && cd hasty
cargo build --release
install -m755 target/release/hasty ~/.local/bin/hasty   # or any PATH dir
hasty
```

First launch runs the setup wizard. Windows, macOS and Linux are supported;
see [Platform notes](#platform-notes).

## First-run setup

1. **Provider** — OpenAI, Anthropic, Google, OpenRouter, local Ollama, or any
   OpenAI-compatible endpoint (custom base URL).
2. **API key** — pasted masked, validated with a real request. Never printed.
3. **Model** — fetched from the provider; pick the main model.
4. **Permission mode** — *Ask every time* (default) or *Full access*. Both are
   changeable later in `/settings`; the catastrophic deny-list applies in both.
5. **Extra keys for subagents** (optional, unlimited) — each with provider, key,
   model and a name/role label.
6. **Connectors** (optional) — GitHub personal access token for skill installs.

Keys go into the OS keychain when `secret-tool` (Linux) or `security` (macOS) is
available, and always into an AES-256-GCM encrypted vault at
`~/.hasty/vault.json` (mode `0600`). The vault key is derived from a machine id
plus a random salt, so copying the file to another machine does not reveal it.
On Windows the encrypted file is used.

## Interface

- Full-screen dark UI, dark-red accent, compact `HASTY` wordmark with version,
  provider and model.
- Multi-line input bar: `Enter` sends, `Alt+Enter`/`Ctrl+J` inserts a newline,
  `Up`/`Down` is history, bracketed paste works.
- `/` opens an autocomplete menu. Arrow keys navigate it and **every menu row is
  clickable with the mouse**; typing the full command also works.
- Each model step gets a collapsible **Thinking** block that streams live
  reasoning (when the provider exposes it). Collapsed by default with a one-line
  summary; click the header or press `Ctrl+T` (`Ctrl+O` for all).
- Tool calls show status icon, arguments, duration, output and colored diffs.
  Click a tool header to expand truncated output.
- `Esc`/`Ctrl+C` interrupts the agent mid-stream. `Esc` twice exits.

| Key | Action |
| --- | --- |
| `Enter` | send message / select menu item |
| `Alt+Enter`, `Ctrl+J` | newline |
| `Up` / `Down` | history, or menu navigation |
| `Tab` | complete the highlighted slash command |
| `Esc` / `Ctrl+C` | interrupt; `Esc` twice exits |
| `Ctrl+T` | toggle the last thinking block |
| `Ctrl+O` | toggle all thinking blocks |
| `Ctrl+P` | open the command list |
| `PgUp` / `PgDn`, wheel | scroll transcript |

## Slash commands

| Command | What it does |
| --- | --- |
| `/model` | pick provider + model (and add provider keys) |
| `/theme` | presets, or `/theme set accent #c8352c` |
| `/theme set <slot> <#hex>` | slots: bg, panel, fg, accent, accent_dim, border, muted, ok, warn, err, user, diff_add, diff_del |
| `/subagents` | running subagents, configured keys, add/remove keys |
| `/settings` | permissions, keys, connectors, paths, outside-project reads |
| `/skills` | install/list/enable/remove skills |
| `/connectors` | GitHub token and other connectors |
| `/sessions` | browse, resume (`Enter`) and delete (`d`) sessions |
| `/sessions export` | write a JSON export of the current session |
| `/clear` | clear conversation context (DB transcript kept) |
| `/compact` | summarize context into a summary message |
| `/init` | generate `HASTY.md` project notes |
| `/help` | keys, mouse and commands |
| `/exit` | quit |

## Agent loop

Iterative, not one-shot:

```
plan → tool call → observe → reflect → re-plan → … → verify → answer
```

- Tools: `read_file`, `write_file`, `edit_file`, `move_file`, `delete_file`,
  `make_dir`, `list_dir`, `glob`, `grep`, `run_command`, `install_package`,
  `open_app`, `git`, `web_fetch`, `use_skill`, `spawn_subagent`, `finish`.
- Every action passes through one gate (`src/permissions.rs`). Ask mode confirms
  commands, installs, app launches, network access and **every file edit**, and
  shows the diff in the prompt before it writes. Credential paths and files
  (`~/.ssh`, `~/.aws`, `.env*`, `*.pem`, `id_rsa`, …) and Hasty's own vault
  always confirm, even in Full access.
- Catastrophic commands are denied outright in both modes. Destructive but
  legitimate commands (`git reset --hard`, force-push, `git clean -fd`) confirm
  instead of being refused. The deny-list sees through `sh -c` wrappers and
  `find / -delete`.
- Tool output is redacted for key-like strings before it is shown, stored or
  sent to the provider.
- Writes are atomic (temp file plus rename) and keep the original file mode, so
  a crash cannot truncate a file and `+x` survives an edit.
- Interrupts stop streaming and kill the whole process group, so a timed-out
  `npm run` does not leave children behind.

## Subagents

The main model can call `spawn_subagent(name, task, model)`. Each subagent:

- gets its own key and model from `/subagents`, its own context and its own
  thread, and runs in parallel with others;
- uses read-only tools, plus writes and commands **only in Full access**;
- reports a concise result that the main agent merges.

If its model errors, is rate-limited or disappears, the subagent fails over to
another model on the same key and records the switch in its report and health
field. `/subagents` shows name, model, status, task and health.

Free-tier detection: Hasty watches provider error bodies and rate-limit headers.
When a key looks free-tier, it marks the provider, restricts model choice to
free-eligible models (e.g. OpenRouter `:free`, all Ollama models) and warns you
about limits in the transcript.

## Skills

Skills are instruction packs (`SKILL.md`) installed from GitHub into
`~/.hasty/skills/repos/<owner>__<repo>`.

- `/skills` lists installed skills, toggles them, and offers suggestions:
  - [`addyosmani/agent-skills`](https://github.com/addyosmani/agent-skills) —
    26 workflow skills (spec, plan, build, test, review, ship); verified
    `skills/<name>/SKILL.md` layout.
  - [`anthropics/skills`](https://github.com/anthropics/skills) — reference
    skills for documents, spreadsheets and artifacts.
  - [`obra/superpowers`](https://github.com/obra/superpowers) — brainstorming,
    debugging and verification discipline.
- `/skills install <github-url>` installs any repo with `SKILL.md` files.
- `/skills remove <owner/repo>` deletes the clone.
- Only a compact index (name + description) is loaded up front. Full instructions
  load on demand through the `use_skill` tool.

GitHub tokens are used only for `git clone`/`git fetch` via a config header, not
in the URL or logs. Device login needs an OAuth client id and is not bundled;
use a personal access token.

## Storage & privacy

Everything is under `~/.hasty/` (override with `HASTY_HOME`):

| File | Purpose |
| --- | --- |
| `config.json` | settings, provider list (no keys) |
| `hasty.db` | SQLite: sessions, messages, tool calls, subagent runs, settings |
| `vault.json` | AES-256-GCM encrypted keys, `0600` |
| `personality.md` | Hasty's editable personality / system voice |
| `skills/` | cloned skill repositories |
| `logs/` | reserved |

CLI maintenance:

```bash
hasty sessions                        # list saved sessions
hasty export [file.json] [--session id]   # export everything or one session
hasty delete <session-id|all>         # delete sessions and their messages
hasty setup                           # re-run the wizard
```

No analytics, no crash reporting, no remote logging. The only network traffic is
to the provider you configured, `github.com` when you install skills, and URLs
you fetch with `web_fetch` (which asks first in Ask mode).

## Personality

`~/.hasty/personality.md` is plain Markdown and is read on every turn. Edit it to
change her voice, stance or boundaries — it is the first section of the system
prompt. Project-specific context goes in `HASTY.md` at the repository root
(`/init` generates a starting point). Hasty also reads `AGENTS.md`, `CLAUDE.md`,
`.cursorrules` and `.github/copilot-instructions.md`, so an existing repository
works without a second copy of its rules.

Skills can also live in the repository at `.hasty/skills/<name>/SKILL.md`; those
load with `use_skill` alongside installed GitHub skills, and `repo:name`
disambiguates duplicates.

## Configuration

`~/.hasty/config.json` (written atomically, `0600`) holds:

- `provider`, `model`, `providers[]` (id, base_url, key_ref, models, free_tier)
- `permission_mode` (`ask_every_time` | `full_access`)
- `theme` (name + per-slot hex overrides)
- `subagent_keys[]` (name, role, provider, key_ref, model)
- `skill_repos[]`, `enabled_skills[]`, `connectors[]`
- `max_iterations` (default 24), `allow_outside_project_reads`
- `secret_backend` (`keychain` | `file`)

Keys are referenced by name (`provider:openai`, `subagent:reviewer`,
`connector:github`); the values live only in the vault.

## Architecture

```
src/
  main.rs          CLI entry (TUI, setup, sessions, export, delete)
  lib.rs           crate root, version, logo
  config.rs        settings, provider/subagent/connector types
  paths.rs         ~/.hasty layout, 0600/0700 helpers
  secrets.rs       keychain + AES-256-GCM vault, masking/redaction
  db.rs            rusqlite schema and queries
  permissions.rs   the single gate + catastrophic deny-list
  theme.rs         presets and per-slot overrides
  personality.rs   personality file + system prompt assembly
  providers/       OpenAI-compatible, Anthropic, Google (streaming SSE), models,
                   retries, error classification, free-tier detection
  tools.rs         file/search/shell/install/app/git/web tools, diffs
  agent/           agent loop, permission prompts, compaction
  agent/subagents.rs  parallel loops, failover, health
  skills.rs        GitHub skill install/parse/enable/load
  commands.rs      slash metadata, /init notes, exports
  tui/             state/input/menus + renderer (mouse hit boxes)
  wizard.rs        first-run setup
tests/             permissions, failover, database
```

## Tests

```bash
cargo test          # gate, failover, DB, tool and provider unit tests
cargo clippy --all-targets
```

## Platform notes

- Linux and macOS: keychain via `secret-tool` / `security`, encrypted vault
  fallback. Shell commands run with `sh -c`.
- Windows: encrypted vault only (no keychain CLI), `cmd /C` for commands,
  `start` for `open_app`. Terminal must support ANSI and mouse reporting
  (Windows Terminal does; the legacy console does not).
- No local build tools are required: SQLite is vendored.
- Terminal size: works from 80×24; the wordmark shortens on short terminals.

## License

No license has been chosen yet. See [LICENSE](LICENSE) (placeholder).
