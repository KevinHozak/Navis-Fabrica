fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_settings",
            "save_settings",
            "start_project_scan",
            "cancel_project_scan",
            "inspect_providers",
            "probe_antigravity_cli",
            "probe_ollama",
            "create_provider_handoff",
            "start_ollama_workflow_run",
            "start_antigravity_workflow_run",
            "list_github_issues",
            "get_github_issue",
        ]),
    ))
    .expect("failed to build Tauri permission manifest");
}
