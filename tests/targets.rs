use std::{fs, process::Command};

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
