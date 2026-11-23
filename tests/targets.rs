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
