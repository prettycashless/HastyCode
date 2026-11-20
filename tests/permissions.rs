//! The permission gate is the security boundary for every tool call.

use std::path::PathBuf;

use hasty::permissions::{catastrophic, risky, Action, Gate, Mode, Outcome};

fn gate(mode: Mode) -> Gate {
    Gate::new(mode, "/work/project")
}

#[test]
fn ask_mode_confirms_commands_installs_and_apps() {
    let g = gate(Mode::AskEveryTime);
    for action in [
        Action::RunCommand {
            command: "cargo test".into(),
        },
        Action::Install {
            what: "lodash".into(),
        },
        Action::OpenApp { app: "code".into() },
        Action::Network {
            url: "https://example.com".into(),
        },
    ] {
        assert!(
            matches!(g.check(&action), Outcome::Confirm(_)),
            "{action:?} should require confirmation"
        );
    }
}

#[test]
fn ask_mode_confirms_edits_and_allows_reads() {
    let g = gate(Mode::AskEveryTime);
    assert!(
        matches!(
            g.check(&Action::WriteFile {
                path: PathBuf::from("/work/project/src/main.rs")
            }),
            Outcome::Confirm(_)
        ),
        "editing a project file must ask in ask mode"
    );
    assert_eq!(
        g.check(&Action::ReadFile {
            path: PathBuf::from("/work/project/README.md")
        }),
        Outcome::Allow
    );
}

#[test]
fn ask_mode_confirms_writes_outside_the_project() {
    let g = gate(Mode::AskEveryTime);
    for path in ["/etc/hosts", "/work/other/file.txt"] {
        assert!(
            matches!(
                g.check(&Action::WriteFile {
                    path: PathBuf::from(path)
                }),
                Outcome::Confirm(_)
            ),
            "{path} should be confirmed"
        );
    }
}

#[test]
fn full_access_runs_without_prompts() {
    let g = gate(Mode::FullAccess);
    assert_eq!(
        g.check(&Action::RunCommand {
            command: "npm run build".into()
        }),
        Outcome::Allow
    );
    assert_eq!(
        g.check(&Action::WriteFile {
            path: PathBuf::from("/work/project/a.txt")
        }),
        Outcome::Allow
    );
}

#[test]
fn catastrophic_commands_are_denied_in_full_access() {
    let g = gate(Mode::FullAccess);
    for command in [
        "rm -rf /",
        "rm -rf /*",
        "rm -rf ~",
        "sudo rm -rf / --no-preserve-root",
        "mkfs.ext4 /dev/sda1",
        "dd if=/dev/zero of=/dev/sda bs=1M",
        ":(){ :|:& };:",
        "shred /dev/sda",
        "diskpart clean",
    ] {
        match g.check(&Action::RunCommand {
            command: command.into(),
        }) {
            Outcome::Deny(_) => {}
            other => panic!("{command} => {other:?}, expected Deny"),
        }
    }
}

#[test]
fn catastrophic_detection_ignores_harmless_commands() {
    for command in [
        "rm -rf target",
        "rm notes.txt",
        "cargo test",
        "git status",
        "ls /",
    ] {
        assert!(
            catastrophic(command).is_none(),
            "{command} should be allowed"
        );
    }
}

#[test]
fn destructive_git_needs_a_second_look() {
    assert!(risky("git push --force origin main").is_some());
    assert!(risky("git reset --hard HEAD~3").is_some());
    // Neither is catastrophic: the gate confirms rather than refusing.
    assert!(catastrophic("git reset --hard HEAD~3").is_none());
    assert!(matches!(
        gate(Mode::FullAccess).check(&Action::RunCommand {
            command: "git reset --hard HEAD~1".into()
        }),
        Outcome::Confirm(_)
    ));
}

#[test]
fn deny_list_sees_through_wrappers() {
    for command in [
        "sh -c 'rm -rf /'",
        "bash -c \"rm -rf ~\"",
        "find / -delete",
        "find ~ -exec rm {} +",
    ] {
        assert!(
            matches!(
                gate(Mode::FullAccess).check(&Action::RunCommand {
                    command: command.into()
                }),
                Outcome::Deny(_)
            ),
            "{command} should be denied"
        );
    }
}

#[test]
fn credential_files_always_confirm() {
    let home = dirs::home_dir().expect("home");
    for mode in [Mode::FullAccess, Mode::AskEveryTime] {
        for path in [
            PathBuf::from("/work/project/.env"),
            PathBuf::from("/work/project/.env.local"),
            PathBuf::from("/work/project/server.pem"),
            home.join("id_rsa"),
        ] {
            assert!(
                matches!(
                    gate(mode).check(&Action::ReadFile { path: path.clone() }),
                    Outcome::Confirm(_)
                ),
                "{mode:?} should confirm {}",
                path.display()
            );
        }
    }
}

#[test]
fn credential_paths_always_confirm() {
    let home = dirs::home_dir().expect("home");
    for mode in [Mode::FullAccess, Mode::AskEveryTime] {
        let outcome = gate(mode).check(&Action::ReadFile {
            path: home.join(".ssh/id_ed25519"),
        });
        assert!(
            matches!(outcome, Outcome::Confirm(_)),
            "{mode:?} should confirm reading SSH keys, got {outcome:?}"
        );
    }
}

#[test]
fn project_boundary_is_lexically_normalized() {
    let g = gate(Mode::AskEveryTime);
    assert!(!g.inside_project(&PathBuf::from("/work/project/../../etc/passwd")));
    assert!(g.inside_project(&PathBuf::from("/work/project/src/../src/lib.rs")));
    assert!(!g.inside_project(&PathBuf::from("/work/project2/file")));
}
