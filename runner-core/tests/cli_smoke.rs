use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], input: Option<&Value>) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fabrica-runner"))
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

fn args<'a>(command: &'a str, project: &'a str, config: &'a str) -> Vec<&'a str> {
    vec![command, "--project", project, "--config", config]
}

#[test]
fn binary_protocol_supports_paths_with_spaces_and_real_issue_crud() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Project with spaces");
    std::fs::create_dir_all(&project).unwrap();
    let config = project.join("runner config.json");
    std::fs::write(
        &config,
        r#"{"schemaVersion":1,"issueScope":"FAB","issuesDir":"issues"}"#,
    )
    .unwrap();
    let project = project.to_str().unwrap();
    let config = config.to_str().unwrap();

    let (code, doctor) = run(&args("doctor", project, config), None);
    assert_eq!(code, 0);
    assert_eq!(doctor["ok"], true);
    assert!(
        !std::path::Path::new(project).join("issues").exists(),
        "doctor must not create storage"
    );
    let record = json!({"frontmatter":{"schema_version":1,"title":"CLI smoke","type":"task","status":"ready","parent":[],"depends_on":[]},"body":"# CLI smoke\n"});
    let (code, created) = run(&args("issues-create", project, config), Some(&record));
    assert_eq!(code, 0, "{created}");
    assert_eq!(created["result"]["record"]["frontmatter"]["id"], "FAB-S001");
    let (code, list) = run(&args("issues-list", project, config), None);
    assert_eq!(code, 0);
    assert_eq!(list["result"].as_array().unwrap().len(), 1);
    let id = created["result"]["record"]["frontmatter"]["id"]
        .as_str()
        .unwrap();
    let (code, plan) = run(
        &["plan", "--project", project, "--config", config, "--id", id],
        None,
    );
    assert_eq!(code, 0);
    assert_eq!(plan["result"]["dryRun"], true);
    assert_eq!(plan["result"]["executionSupported"], false);
    let revision = created["result"]["revision"].as_str().unwrap();
    let (code, deleted) = run(
        &[
            "issues-delete",
            "--project",
            project,
            "--config",
            config,
            "--id",
            id,
            "--expected-revision",
            revision,
        ],
        None,
    );
    assert_eq!(code, 0);
    assert_eq!(deleted["result"]["deleted"], true);
    let (code, caps) = run(&["capabilities"], None);
    assert_eq!(code, 0);
    assert_eq!(caps["result"]["protocolVersion"], 1);
    assert_eq!(caps["result"]["providerCalls"], true);
    assert_eq!(
        caps["result"]["paidProviderCalls"],
        "blockedWithoutEnforceableRunBudget"
    );
}

#[test]
fn binary_returns_stable_protocol_and_bounded_input_errors() {
    let (code, bad) = run(
        &["issues-create"],
        Some(&json!({"frontmatter":{},"body":""})),
    );
    assert_eq!(code, 2);
    assert_eq!(bad["error"]["code"], "INVALID_ARGUMENT");
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("p");
    std::fs::create_dir_all(&project).unwrap();
    let config = project.join("runner.json");
    std::fs::write(&config, "{").unwrap();
    let (code, invalid) = run(
        &args(
            "doctor",
            project.to_str().unwrap(),
            config.to_str().unwrap(),
        ),
        None,
    );
    assert_eq!(code, 2);
    assert_eq!(invalid["error"]["code"], "INVALID_ARGUMENT");

    let oversized =
        json!({"frontmatter":{"schema_version":1,"title":"x".repeat(1024 * 1024)},"body":""});
    let (code, too_large) = run(&["issues-create"], Some(&oversized));
    assert_eq!(code, 2);
    assert_eq!(too_large["error"]["code"], "INPUT_TOO_LARGE");
}

#[test]
fn provider_commands_read_the_same_versioned_settings_without_secret_values() {
    let temp = tempfile::tempdir().unwrap();
    let settings = temp.path().join("settings with spaces.json");
    std::fs::write(&settings, serde_json::json!({
        "schemaVersion":4,
        "projectRoot":temp.path(),
        "exclusions":[".git"],
        "preferences":{"includeHidden":false,"scanDepth":3},
        "providers":[{"id":"custom-one","kind":"custom","name":"Fixture","executable":"ignored","secretRef":"credential-store:fixture-session","models":[]}],
        "providerSelection":{"providerId":"custom-one","modelId":null,"reasoningEffort":null,"runtimeId":null},
        "githubAccounts":[],"selectedGithubAccountId":null,"typesafeProfile":null
    }).to_string()).unwrap();
    let settings = settings.to_str().unwrap();
    let (code, listed) = run(&["provider-settings", "--settings", settings], None);
    assert_eq!(code, 0, "{listed}");
    assert_eq!(
        listed["result"]["schemaVersion"],
        navis_fabrica_runner_core::settings::SETTINGS_SCHEMA_VERSION
    );
    assert_eq!(
        listed["result"]["providers"][0]["secretRef"],
        "credential-store:fixture-session"
    );
    assert!(!listed.to_string().contains("sk-or-secret"));
    let (code, doctor) = run(&["providers-doctor", "--settings", settings], None);
    assert_eq!(code, 0, "{doctor}");
    assert_eq!(
        doctor["result"]["providers"][0]["state"],
        "unsupportedProvider"
    );
}
