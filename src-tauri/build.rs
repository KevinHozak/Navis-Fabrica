fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_settings",
            "save_settings",
            "start_project_scan",
            "cancel_project_scan",
        ]),
    ))
    .expect("failed to build Tauri permission manifest");
}
