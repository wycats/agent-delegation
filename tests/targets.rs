use std::{fs, process::Command};

#[test]
fn named_and_direct_provider_commands_launch_their_own_runner() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("targets.toml");
    fs::write(&config, "[targets.personal]\nbackend='google'\nmodel='g'\n[targets.writer]\nbackend='claude'\nmodel='c'\n").unwrap();
    for (backend, name, expected) in [
        ("google", "personal", "agent-delegation google run "),
        ("claude", "writer", "Usage: agent-delegation claude "),
    ] {
        for args in [vec![backend, "--help"], vec!["--target", name, "--help"]] {
            let output = Command::new(env!("CARGO_BIN_EXE_agent-delegation"))
                .env("AGENT_DELEGATION_CONFIG", &config)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}");
            let help = String::from_utf8(output.stdout).unwrap();
            assert!(help.starts_with(expected), "{args:?}: {help}");
        }
    }
}

#[cfg(unix)]
#[test]
fn named_ahp_editor_and_profile_are_independent_of_shell_overrides() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("targets.toml");
    let log = dir.path().join("discovery");
    let shell_code = ("AGENT_DELEGATION_CODE", "ambient editor");
    let shell_profile = ("AGENT_DELEGATION_USER_DATA_DIR", "--ambient profile");
    // Stop after the first discovery command and record exactly what the binary invoked.
    for editor in ["code", "configured editor", "ambient editor"] {
        let path = dir.path().join(editor);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{editor}' \"$@\" > \"$DISCOVERY_LOG\"\nprintf '[]\\n'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for code in [None, Some("configured editor")] {
        for profile in [None, Some("profile λ with spaces")] {
            let mut definition =
                String::from("[targets.work]\nbackend='ahp'\nsession='s'\nchat='c'\nmodel='m'\n");
            if let Some(code) = code {
                definition.push_str(&format!("code='{code}'\n"));
            }
            if let Some(profile) = profile {
                definition.push_str(&format!("user_data_dir='{profile}'\n"));
            }
            fs::write(&config, definition).unwrap();
            let mut expected = vec![code.unwrap_or("code"), "agent", "ps", "--all", "--json"];
            if let Some(profile) = profile {
                expected.extend(["--user-data-dir", profile]);
            }
            for overrides in [
                vec![],
                vec![shell_code],
                vec![shell_profile],
                vec![shell_code, shell_profile],
            ] {
                let output = Command::new(env!("CARGO_BIN_EXE_agent-delegation"))
                    .arg("--config")
                    .arg(&config)
                    .args(["--target", "work", "status"])
                    .env("PATH", dir.path())
                    .env("DISCOVERY_LOG", &log)
                    .env_remove(shell_code.0)
                    .env_remove(shell_profile.0)
                    .envs(overrides.iter().copied())
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "fixture has no sessions");
                let observed = fs::read_to_string(&log).unwrap();
                assert_eq!(
                    observed.lines().collect::<Vec<_>>(),
                    expected,
                    "target ({code:?}, {profile:?}), shell {overrides:?}"
                );
                fs::remove_file(&log).unwrap();
            }
        }
    }
}

#[test]
fn named_targets_bind_native_runs_and_leave_direct_commands_independent() {
    let dir = std::env::temp_dir().join(format!("targets-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let config = dir.join("targets.toml");
    fs::write(&config, "[targets.personal]\nbackend='google'\nmodel='gemini-test'\n[targets.writer]\nbackend='claude'\nmodel='claude-test'\n").unwrap();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-delegation"))
            .env("AGENT_DELEGATION_CONFIG", &config)
            .args(args)
            .output()
            .unwrap()
    };
    let list = cli(&["targets"]);
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "personal\tgoogle\tgemini-test",
            "writer\tclaude\tclaude-test"
        ]
    );
    assert!(!cli(&["--target", "unknown", "run"]).status.success());
    for name in ["personal", "writer"] {
        let result = cli(&["--target", name, "run", "--model", "another-model"]);
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !result.status.success() && message.contains("Model differs"),
            "{message}"
        );
    }
    // Direct provider invocations inherit no model binding, including when a
    // caller happens to have the internal runner variable in its environment.
    for backend in ["google", "claude"] {
        let run = |ambient: bool| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-delegation"));
            cmd.args([backend, "run", "--model", "another-model"])
                .env_remove("AGENT_DELEGATION_TARGET_MODEL");
            if ambient {
                cmd.env("AGENT_DELEGATION_TARGET_MODEL", "ambient");
            }
            cmd.output().unwrap()
        };
        let clean = run(false);
        let ambient = run(true);
        assert!(!clean.status.success());
        assert_eq!(clean, ambient);
    }
    fs::write(
        dir.join("intent.json"),
        r#"{"provider":"antigravity","model":"claude-test"}"#,
    )
    .unwrap();
    for name in ["personal", "writer"] {
        let result = cli(&[
            "--target",
            name,
            "review",
            "--run-dir",
            dir.to_str().unwrap(),
            "--note",
            "check",
        ]);
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !result.status.success() && message.contains("does not belong"),
            "{message}"
        );
    }
    fs::write(
        &config,
        "[targets.personal]\nbackend='google'\nmodel='gemini-test'\nmodle='typo'",
    )
    .unwrap();
    assert!(!cli(&["targets"]).status.success());
    assert!(cli(&["ahp", "--help"]).status.success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn saved_target_keeps_the_receipt_location_and_chat_identity_in_one_place() {
    let dir = tempfile::tempdir().unwrap();
    let registry = dir.path().join("targets.toml");
    let definition = "[targets.work-neon]\nbackend = 'ahp'\nassignment = { config = 'config.json', receipt = 'receipt.json' }\n";
    fs::write(&registry, definition).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-delegation"))
            .arg("--config")
            .arg(&registry)
            .args(args)
            .output()
            .unwrap()
    };
    let path = dir.path();
    let help = run(&["--target", "work-neon", "help"]);
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("submit <prompt-file>")
    );
    let target = serde_json::json!({"cli":path.join("code"), "profile":path,"instanceId":"editor","workspace":path,
        "model":{"id":"work/model","thinkingLevel":"high"}});
    fs::write(path.join("config.json"), target.to_string()).unwrap();
    let receipt = serde_json::json!({"target":target,"session":"ahp-session:/saved","chat":"ahp-chat:/saved","clientId":"owner"});
    let durable = path.join("receipt.json");
    fs::write(&durable, receipt.to_string()).unwrap();
    let list = run(&["targets"]);
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout).unwrap().trim(),
        "work-neon\tahp\twork/model"
    );
    for args in [
        vec!["submit"],
        vec!["watch", "another-receipt"],
        vec!["check", "other-profile"],
    ] {
        let mut call = vec!["--target", "work-neon"];
        call.extend(args);
        assert!(!run(&call).status.success());
    }
    fs::write(&registry, format!("{definition}model = 'overridden'\n")).unwrap();
    assert!(!run(&["targets"]).status.success());
    assert_eq!(fs::read_to_string(durable).unwrap(), receipt.to_string());
}
