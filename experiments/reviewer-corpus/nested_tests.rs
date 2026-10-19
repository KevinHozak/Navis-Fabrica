#[cfg(test)]
mod corpus_tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::tempdir;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nCORPUS_MARKER";
    fn put(root: &Path, relative: &str, bytes: &[u8]) {
        let p = root.join(relative);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    fn expected(bytes: &[u8]) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        )
    }

    #[test]
    fn invariant_root_logo_is_selected() {
        let d = tempdir().unwrap();
        let missing = d.path().join("missing-project");
        assert_eq!(data_url(&missing), None);
        let oversized = tempdir().unwrap();
        let mut large_png = vec![b'X'; 512 * 1024 + 1];
        large_png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        put(oversized.path(), "assets/logo.png", &large_png);
        assert_eq!(data_url(oversized.path()), None);

        put(d.path(), "assets/logo.png", PNG);
        assert_eq!(data_url(d.path()), Some(expected(PNG)));
    }

    #[test]
    fn contract_frontend_public_icon_is_selected() {
        let d = tempdir().unwrap();
        put(d.path(), "frontend/public/icon-512.png", PNG);
        assert_eq!(data_url(d.path()), Some(expected(PNG)));
    }

    #[test]
    fn contract_project_light_mark_is_selected() {
        let d = tempdir().unwrap();
        let project = d.path().join("demo");
        put(&project, "apps/frontend/public/demo-light-mark.png", PNG);
        assert_eq!(data_url(&project), Some(expected(PNG)));
    }
}
