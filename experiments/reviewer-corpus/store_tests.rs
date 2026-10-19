#[cfg(test)]
mod corpus_tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::tempdir;

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
    fn invariant_readme_logo_keeps_first_precedence() {
        let d = tempdir().unwrap();
        let missing = d.path().join("missing-project");
        assert_eq!(data_url(&missing), None);
        let oversized = tempdir().unwrap();
        let mut large_png = vec![b'X'; 512 * 1024 + 1];
        large_png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        put(oversized.path(), "assets/logo.png", &large_png);
        assert_eq!(data_url(oversized.path()), None);

        let logo = b"\x89PNG\r\n\x1a\nREADME_LOGO_MARKER";
        put(d.path(), "assets/logo.png", logo);
        put(
            d.path(),
            "assets/play-store-icon.png",
            b"\x89PNG\r\n\x1a\nSTORE_MARKER",
        );
        assert_eq!(data_url(d.path()), Some(expected(logo)));
    }

    #[test]
    fn contract_oversized_generic_icon_uses_store_art() {
        let d = tempdir().unwrap();
        let current = b"\x89PNG\r\n\x1a\nCURRENT_STORE_MARKER";
        let mut oversized = vec![b'X'; 512 * 1024 + 1];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        put(d.path(), "assets/icon.png", &oversized);
        put(d.path(), "assets/play-store-icon.png", current);
        put(
            d.path(),
            "public/icon-192.png",
            b"\x89PNG\r\n\x1a\nSTALE_PUBLIC_MARKER",
        );
        assert_eq!(data_url(d.path()), Some(expected(current)));
    }
}
