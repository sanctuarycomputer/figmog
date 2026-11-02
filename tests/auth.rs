#![recursion_limit = "256"]

mod common;

use serde_json::{Value, json};

#[test]
fn network_pull_explains_how_to_restore_saved_credentials_after_an_empty_override() {
    let cwd = tempfile::tempdir().unwrap();
    let out = common::command()
        .current_dir(cwd.path())
        .args(["pull", "syntheticfile123"])
        .assert()
        .failure()
        .code(1);
    let output = out.get_output();
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("FIGMA_TOKEN is empty"));
    assert!(message.contains("disables saved credentials"));
    assert!(message.contains("unset FIGMA_TOKEN to use saved credentials"));
    assert!(message.contains("figmog auth login"));
    assert!(message.contains("figmog auth status"));
    assert!(!cwd.path().join(".figmog").exists());
}

#[test]
fn auth_status_works_without_a_mirror_and_never_prints_the_token() {
    let cwd = tempfile::tempdir().unwrap();
    let out = common::command()
        .current_dir(cwd.path())
        .env("FIGMA_TOKEN", "figd_synthetic_private_token")
        .args(["auth", "status"])
        .assert()
        .success();
    let output = out.get_output();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        status,
        json!({
            "configured": true,
            "source": "environment",
            "validated": false,
        })
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("figd_synthetic_private_token"));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);

    let out = common::command()
        .current_dir(cwd.path())
        .args(["auth", "status"])
        .assert()
        .success();
    let status: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        status,
        json!({"configured": false, "source": null, "validated": false})
    );
}

#[test]
fn login_rejects_invalid_stdin_without_exposing_it_or_accessing_the_keychain() {
    for token in [
        "".to_string(),
        "figd_first\nfigd_secret_second\n".into(),
        "x".repeat(4099),
    ] {
        let out = common::command()
            .args(["auth", "login", "--token-stdin"])
            .write_stdin(token)
            .assert()
            .failure()
            .code(1);
        let output = out.get_output();
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(error["error"].is_string());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("figd_secret_second"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("credential store"));
    }
}

#[test]
fn login_requires_explicit_stdin_mode_and_rejects_token_arguments() {
    let out = common::command()
        .args(["auth", "login"])
        .write_stdin("figd_do_not_echo")
        .assert()
        .failure()
        .code(1);
    let output = out.get_output();
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("--token-stdin"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("figd_do_not_echo"));
    common::command()
        .args(["auth", "login", "--token", "figd_not_an_argument"])
        .assert()
        .failure()
        .code(2);
}
