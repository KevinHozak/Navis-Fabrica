use base64::Engine;
use std::fs;
use std::path::Path;

const MAX_ICON_BYTES: u64 = 512 * 1024;
const APP_ROOTS: &[&str] = &[
    "",
    "frontend",
    "web",
    "client",
    "app",
    "packages/web",
    "apps/web",
    "packages/frontend",
    "apps/frontend",
];
const ICON_CANDIDATES: &[&str] = &[
    "assets/logo.png",
    "assets/logo.webp",
    "assets/logo.jpg",
    "assets/logo.svg",
    "logo.png",
    "logo.svg",
    "public/logo.png",
    "public/logo.webp",
    "public/logo.jpg",
    "public/logo.svg",
    "src-tauri/icons/icon.png",
    "src-tauri/icons/icon.ico",
    "public/favicon.png",
    "public/favicon.ico",
    "public/icon.png",
    "public/icon-512.png",
    "public/icon-192.png",
    "src/app/icon.png",
    "app/icon.png",
    "assets/icon.png",
    "assets/favicon.png",
    "icon.png",
    "favicon.png",
    "favicon.ico",
];

/// Return an embedded image URL for a conventional project app icon, if present.
/// Only small, supported image files within the canonical project folder are accepted.
pub fn data_url(project_path: &Path) -> Option<String> {
    let root = project_path.canonicalize().ok()?;
    if !root.is_dir() {
        return None;
    }

    let project_name = root.file_name()?.to_str()?.to_ascii_lowercase();
    let mut candidates = Vec::new();
    for app_root in APP_ROOTS {
        let app_root = root.join(app_root);
        if !app_root.is_dir() {
            continue;
        }
        candidates.extend(
            ICON_CANDIDATES
                .iter()
                .map(|relative| app_root.join(relative)),
        );
        // Some applications use a project-named brand mark rather than logo.png.
        // Probe exact conventional names instead of recursively scanning artwork.
        for folder in ["assets", "public"] {
            for label in ["logo", "icon", "mark", "light-mark"] {
                for extension in ["png", "svg", "webp", "jpg", "ico"] {
                    candidates.push(
                        app_root
                            .join(folder)
                            .join(format!("{project_name}-{label}.{extension}")),
                    );
                }
            }
        }
    }
    for path in candidates {
        let canonical = match path.canonicalize() {
            Ok(path) if path.starts_with(&root) => path,
            _ => continue,
        };
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata)
                if metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_ICON_BYTES =>
            {
                metadata
            }
            _ => continue,
        };
        let Ok(bytes) = fs::read(canonical) else {
            continue;
        };
        if bytes.len() as u64 != metadata.len() {
            continue;
        }
        let Some(mime) = image_mime(&bytes) else {
            continue;
        };
        return Some(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ));
    }
    None
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 6 && bytes[..4] == [0, 0, 1, 0] {
        Some("image/x-icon")
    } else if safe_svg(bytes) {
        Some("image/svg+xml")
    } else {
        None
    }
}

fn safe_svg(bytes: &[u8]) -> bool {
    let Ok(svg) = std::str::from_utf8(bytes) else {
        return false;
    };
    let lower = svg.to_ascii_lowercase();
    let Some(svg_start) = lower.find("<svg") else {
        return false;
    };
    svg_start <= 1024
        && ![
            "<script",
            "<foreignobject",
            "<!doctype",
            "<!entity",
            "javascript:",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        && !lower.contains("href=\"http")
        && !lower.contains("href='http")
        && !lower.contains("url(http")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolves_a_supported_app_icon_and_ignores_missing_or_invalid_files() {
        let project = tempdir().unwrap();
        assert_eq!(data_url(project.path()), None);

        let icons = project.path().join("src-tauri/icons");
        fs::create_dir_all(&icons).unwrap();
        fs::write(icons.join("icon.png"), b"not an image").unwrap();
        fs::write(project.path().join("icon.png"), b"\x89PNG\r\n\x1a\nexample").unwrap();

        let result = data_url(project.path()).unwrap();
        assert!(result.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn prefers_the_readme_ready_logo_location_when_present() {
        let project = tempdir().unwrap();
        let tauri_icons = project.path().join("src-tauri/icons");
        fs::create_dir_all(&tauri_icons).unwrap();
        fs::write(
            tauri_icons.join("icon.png"),
            b"\x89PNG\r\n\x1a\nplatform-icon",
        )
        .unwrap();
        let assets = project.path().join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("logo.png"), b"\x89PNG\r\n\x1a\nreadme-logo").unwrap();

        let result = data_url(project.path()).unwrap();
        assert!(result.ends_with(
            &base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nreadme-logo")
        ));
    }

    #[test]
    fn rejects_icons_larger_than_the_bound() {
        let project = tempdir().unwrap();
        fs::write(
            project.path().join("icon.png"),
            vec![0; MAX_ICON_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(data_url(project.path()), None);
    }

    #[test]
    fn finds_nested_frontend_icons_and_project_named_monorepo_marks() {
        let directory = tempdir().unwrap();
        let navis = directory.path().join("Navis-Protocol");
        fs::create_dir_all(navis.join("frontend/public")).unwrap();
        fs::write(
            navis.join("frontend/public/icon-512.png"),
            b"\x89PNG\r\n\x1a\nnavis-icon",
        )
        .unwrap();
        assert!(data_url(&navis)
            .unwrap()
            .starts_with("data:image/png;base64,"));

        let mimir = directory.path().join("Mimir");
        fs::create_dir_all(mimir.join("packages/web/public")).unwrap();
        fs::write(
            mimir.join("packages/web/public/mimir-light-mark.svg"),
            b"<svg xmlns='http://www.w3.org/2000/svg'><path d='M0 0'/></svg>",
        )
        .unwrap();
        assert!(data_url(&mimir)
            .unwrap()
            .starts_with("data:image/svg+xml;base64,"));

        fs::create_dir_all(mimir.join("assets")).unwrap();
        fs::write(mimir.join("assets/logo.png"), b"\x89PNG\r\n\x1a\npreferred").unwrap();
        assert!(data_url(&mimir)
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[cfg(unix)]
    #[test]
    fn nested_icon_symlinks_cannot_escape_the_project() {
        let project = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(
            outside.path().join("icon.png"),
            b"\x89PNG\r\n\x1a\nexternal",
        )
        .unwrap();
        fs::create_dir_all(project.path().join("frontend")).unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("frontend/public")).unwrap();
        assert_eq!(data_url(project.path()), None);
    }

    #[test]
    fn accepts_a_self_contained_logo_svg_and_rejects_active_or_remote_content() {
        assert!(safe_svg(
            b"<svg xmlns='http://www.w3.org/2000/svg'><path d='M0 0'/></svg>"
        ));
        assert!(!safe_svg(b"<svg><script>alert(1)</script></svg>"));
        assert!(!safe_svg(
            b"<svg><image href='https://example.test/logo.png'/></svg>"
        ));
    }
}
