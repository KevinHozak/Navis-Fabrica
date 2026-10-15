fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_settings",
            "get_project_inventory",
            "save_settings",
            "start_project_scan",
            "cancel_project_scan",
            "inspect_providers",
            "probe_antigravity_cli",
            "probe_ollama",
            "save_openrouter_credential",
            "probe_openrouter",
            "save_typesafe_credential",
            "probe_typesafe",
            "start_openrouter_workflow_run",
            "create_provider_handoff",
            "start_ollama_workflow_run",
            "start_antigravity_workflow_run",
            "list_github_issues",
            "get_github_issue",
        ]),
    ))
    .expect("failed to build Tauri permission manifest");
}
