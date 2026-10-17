use navis_fabrica_runner_core::{execute, CoreRequest, Operation, PROTOCOL_VERSION};
use std::path::Path;

/// Refresh real provider observations through the public core operation.
/// The configured Codex executable must be a synthetic app-server fixture.
pub fn refresh_provider_observations(data_root: &Path, settings_path: &Path) {
    let response = execute(CoreRequest {
        protocol_version: PROTOCOL_VERSION,
        operation: Operation::RefreshProviderObservations,
        project_path: None,
        config_path: None,
        settings_path: Some(settings_path.to_path_buf()),
        allow_live_probe: false,
        issue_id: None,
        offset: None,
        limit: None,
        expected_revision: None,
        record: None,
        runtime_request: None,
        data_root: Some(data_root.to_path_buf()),
        batch_request: None,
        batch_request_path: None,
        batch_id: None,
        cursor: None,
        worker_executable: None,
        preview_fingerprint: None,
        recovery: None,
    });
    assert!(
        response.ok,
        "synthetic provider-observations-refresh failed: {:?}",
        response.error
    );
    let result = response
        .result
        .expect("successful refresh returns snapshot");
    assert_eq!(result["providers"][0]["authenticated"]["value"], true);
    assert_eq!(result["providers"][0]["healthy"]["value"], true);
    assert_eq!(
        result["providers"][0]["models"][0]["modelAvailable"]["value"],
        true
    );
}

/// Minimal protocol script for tests that only need provider refresh. It does
/// not start a thread/turn and returns only synthetic local fixture data.
pub fn write_observation_app_server(path: &Path, model_id: &str) {
    let model_id = serde_json::to_string(model_id).unwrap();
    let model_json =
        format!("[{{\"id\":{model_id},\"contextWindow\":16384,\"supportsTools\":true}}]");
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$models='{model_json}'
while ($line=[Console]::In.ReadLine()) {{
  $m=$line|ConvertFrom-Json
  switch ($m.method) {{
    'initialize' {{ [Console]::Out.WriteLine('{{"id":1,"result":{{}}}}') }}
    'account/read' {{ [Console]::Out.WriteLine('{{"id":2,"result":{{"account":{{"type":"chatgpt","requiresOpenaiAuth":true}}}}}}') }}
    'model/list' {{ [Console]::Out.WriteLine('{{"id":3,"result":{{"data":' + $models + '}}}}') }}
  }}
}}
"#
    );
    std::fs::write(path, script).expect("write synthetic Codex observation app-server");
}
