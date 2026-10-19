use base64::Engine;
use std::fs;
use std::path::Path;

const MAX_ICON_BYTES: u64 = 512 * 1024;

/// Optional private overrides, keyed by project path, with images confined to the data root.
pub fn override_data_url(data_root: &Path, project_path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Overrides {
        schema_version: u32,
        projects: std::collections::HashMap<String, std::path::PathBuf>,
    }
    let file = match fs::File::open(data_root.join("logo-overrides.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read logo overrides: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65_536 {
        return Err("Logo overrides exceed 64 KiB.".into());
    }
    let overrides: Overrides =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid logo overrides: {e}"))?;
    if overrides.schema_version != 1 {
        return Err("Unsupported logo override schema.".into());
    }
    let project = project_path.canonicalize().map_err(|e| e.to_string())?;
    let mut matching = overrides
        .projects
        .iter()
        .filter(|(path, _)| Path::new(path).canonicalize().ok().as_ref() == Some(&project));
    let Some((_, relative)) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err("Multiple logo overrides match this project.".into());
    }
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(
            "Logo paths must be relative to Fabrica's data folder without parent segments.".into(),
        );
    }
    let root = data_root.canonicalize().map_err(|e| e.to_string())?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("Logo is unavailable: {e}"))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err("Logo must be a file inside Fabrica's data folder.".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_ICON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ICON_BYTES {
        return Err("Logo must be nonempty and no larger than 512 KiB.".into());
    }
    let mime = image_mime(&bytes).ok_or("Unsupported or unsafe logo image.")?;
    Ok(Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )))
}
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
    "assets/icon.png",
    "assets/play-store-icon.png",
    "src-tauri/icons/icon.png",
    "src-tauri/icons/icon.ico",
    "public/favicon.png",
    "public/favicon.ico",
    "public/icon.png",
    "public/icon-512.png",
    "public/icon-192.png",
    "src/app/icon.png",
    "app/icon.png",
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
    use quick_xml::events::Event;

    if bytes.len() as u64 > MAX_ICON_BYTES {
        return false;
    }
    let Ok(svg) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut reader = quick_xml::Reader::from_str(svg);
    let mut depth = 0usize;
    let mut saw_root = false;
    let mut closed_root = false;

    loop {
        let event = reader.read_event();
        let is_empty = matches!(&event, Ok(Event::Empty(_)));
        match event {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if !validate_svg_element(&element) {
                    return false;
                }
                let name = element.name();
                let name = name.as_ref().as_bytes();
                let local_name = name.rsplit(|byte| *byte == b':').next().unwrap_or(name);
                if !saw_root {
                    if local_name != b"svg" || reader.buffer_position() > 1024 {
                        return false;
                    }
                    saw_root = true;
                } else if closed_root {
                    return false;
                }
                if local_name.eq_ignore_ascii_case(b"script")
                    || local_name.eq_ignore_ascii_case(b"foreignObject")
                    || local_name.eq_ignore_ascii_case(b"style")
                    || local_name.eq_ignore_ascii_case(b"animate")
                    || local_name.eq_ignore_ascii_case(b"set")
                    || local_name.eq_ignore_ascii_case(b"animateTransform")
                    || local_name.eq_ignore_ascii_case(b"animateMotion")
                    || local_name.eq_ignore_ascii_case(b"discard")
                {
                    return false;
                }
                if is_empty {
                    if depth == 0 {
                        closed_root = true;
                    }
                } else {
                    depth += 1;
                }
            }
            Ok(Event::End(_)) => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                if depth == 0 {
                    closed_root = true;
                }
            }
            Ok(Event::Text(text)) => {
                if depth == 0 && !text.as_ref().as_bytes().iter().all(u8::is_ascii_whitespace) {
                    return false;
                }
            }
            Ok(Event::CData(text)) => {
                if depth == 0 && !text.as_ref().as_bytes().iter().all(u8::is_ascii_whitespace) {
                    return false;
                }
            }
            Ok(Event::GeneralRef(_)) => return false,
            Ok(Event::Decl(_)) if !saw_root => {}
            Ok(Event::Decl(_)) => return false,
            Ok(Event::DocType(_)) | Ok(Event::PI(_)) => return false,
            Ok(Event::Comment(_)) => {}
            Ok(Event::Eof) => break,
            Err(_) => return false,
        }
    }
    saw_root && closed_root && depth == 0
}

fn validate_svg_element(element: &quick_xml::events::BytesStart<'_>) -> bool {
    let name = element.name();
    let name = name.as_ref().as_bytes();
    let local_name = name.rsplit(|byte| *byte == b':').next().unwrap_or(name);
    if !local_name.eq_ignore_ascii_case(b"svg") && !local_name.iter().all(u8::is_ascii_alphanumeric)
    {
        return false;
    }
    for attribute in element.attributes() {
        let Ok(attribute) = attribute else {
            return false;
        };
        let key = attribute.key.as_ref().as_bytes();
        let key_lower = key.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
        let local_key = key_lower
            .rsplit(|byte| *byte == b':')
            .next()
            .unwrap_or(&key_lower);
        if key_lower.starts_with(b"on") || key_lower == b"xml:base" {
            return false;
        }
        let Ok(value) = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0) else {
            return false;
        };
        let value = value.trim();
        if local_key == b"href" && !is_internal_fragment(value) {
            return false;
        }
        if key_lower == b"style" || is_css_presentation_attribute(&key_lower) {
            if !safe_css_value(value) {
                return false;
            }
        }
    }
    true
}

fn is_internal_fragment(value: &str) -> bool {
    let value = value.trim();
    value
        .strip_prefix('#')
        .is_some_and(|fragment| !fragment.trim().is_empty())
}

fn is_css_presentation_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"fill"
            | b"stroke"
            | b"filter"
            | b"clip-path"
            | b"mask"
            | b"cursor"
            | b"marker"
            | b"marker-start"
            | b"marker-mid"
            | b"marker-end"
    )
}

fn safe_css_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("@import")
        || lower.contains("/*")
        || lower.contains("*/")
        || value.contains('\\')
    {
        return false;
    }
    let mut remaining = lower.as_str();
    while let Some(index) = remaining.find("url") {
        remaining = &remaining[index + 3..];
        let remaining_trimmed = remaining.trim_start();
        if !remaining_trimmed.starts_with('(') {
            remaining = remaining_trimmed;
            continue;
        }
        remaining = &remaining_trimmed[1..];
        let Some(end) = remaining.find(')') else {
            return false;
        };
        let target = remaining[..end]
            .trim()
            .trim_matches(|character| character == '\'' || character == '"')
            .trim();
        if !is_internal_fragment(target) {
            return false;
        }
        remaining = &remaining[end + 1..];
    }
    true
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
    fn prefers_small_store_artwork_over_stale_web_icons_when_native_icon_is_oversized() {
        let project = tempdir().unwrap();
        fs::create_dir_all(project.path().join("assets")).unwrap();
        fs::create_dir_all(project.path().join("public")).unwrap();
        fs::write(
            project.path().join("assets/icon.png"),
            vec![0; MAX_ICON_BYTES as usize + 1],
        )
        .unwrap();
        let current_icon = b"\x89PNG\r\n\x1a\ncurrent-g-icon";
        fs::write(
            project.path().join("assets/play-store-icon.png"),
            current_icon,
        )
        .unwrap();
        fs::write(
            project.path().join("public/icon-192.png"),
            b"\x89PNG\r\n\x1a\nstale-web-icon",
        )
        .unwrap();
        assert_eq!(
            data_url(project.path()).unwrap(),
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(current_icon)
            )
        );
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
    #[test]
    fn parses_svg_and_allows_internal_fragment_resources() {
        assert!(safe_svg(b"<svg xmlns='http://www.w3.org/2000/svg'><defs><linearGradient id='g'/></defs><path d='M0 0' fill=\"url('#g')\"/><use href='#shape'/></svg>"));
        assert!(safe_svg(b"<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink'><use xlink:href='#shape'/></svg>"));
    }

    #[test]
    fn rejects_spaced_remote_and_encoded_resource_references() {
        assert!(!safe_svg(
            b"<svg><image href = 'https://example.test/a.png'/></svg>"
        ));
        assert!(!safe_svg(b"<svg><image href='logo.png'/></svg>"));
        assert!(!safe_svg(
            b"<svg><image href='//example.test/a.png'/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><image href='data:image/png;base64,AAAA'/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><image href='https://example.test/a&amp;b.png'/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><image href=' &#x68;ttps://example.test/a.png '/></svg>"
        ));
        assert!(!safe_svg(b"<svg><image href='file:///tmp/a.png'/></svg>"));
    }

    #[test]
    fn allows_entity_decoded_internal_fragment_references() {
        assert!(safe_svg(b"<svg><use href='&#35;shape'/></svg>"));
        assert!(safe_svg(b"<svg><path fill=\"url( '#paint' )\"/></svg>"));
    }

    #[test]
    fn rejects_unsafe_css_mutations_imports_and_escapes() {
        assert!(!safe_svg(
            b"<svg><path style=\"fill: url(https://example.test/a)\"/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><path style=\"fill: url(logo.png)\"/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><path style=\"@import 'https://example.test/a.css'\"/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><path style=\"fill: url(\\\\23 paint)\"/></svg>"
        ));
        assert!(!safe_svg(b"<svg><path onload='alert(1)'/></svg>"));
        assert!(!safe_svg(b"<svg xml:base='https://example.test/'/>"));
        assert!(!safe_svg(
            b"<svg><animate attributeName='href' to='https://example.test'/></svg>"
        ));
        assert!(!safe_svg(
            b"<svg><style><![CDATA[path { fill: url(https://example.test/a) }]]></style></svg>"
        ));
    }

    #[test]
    fn rejects_invalid_documents_and_active_markup() {
        assert!(!safe_svg(b"<html><svg/></html>"));
        assert!(!safe_svg(b"<svg><path></svg>"));
        assert!(!safe_svg(b"<svg><script>alert(1)</script></svg>"));
        assert!(!safe_svg(b"<svg><foreignObject/></svg>"));
        assert!(!safe_svg(b"<!DOCTYPE svg><svg/>"));
        assert!(!safe_svg(b"<?xml-stylesheet href='x.css'?><svg/>"));
    }

    #[test]
    fn unsafe_logo_falls_through_to_safe_icon_png() {
        let project = tempdir().unwrap();
        let assets = project.path().join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(
            assets.join("logo.svg"),
            b"<svg><image href=' https://example.test/logo.png'/></svg>",
        )
        .unwrap();
        let safe_icon = b"\x89PNG\r\n\x1a\nsafe-icon";
        fs::write(assets.join("icon.png"), safe_icon).unwrap();
        assert_eq!(
            data_url(project.path()).unwrap(),
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(safe_icon)
            )
        );
    }
    #[test]
    fn safe_svg_rejects_spaced_external_double_quoted_href() {
        assert!(!safe_svg(
            br#"<svg><a href = "https://example.com"/></svg>"#
        ));
    }

    #[test]
    fn safe_svg_rejects_spaced_css_url_external_target() {
        assert!(!safe_svg(
            br#"<svg><rect fill='url( "https://example.com" )'/></svg>"#
        ));
    }

    #[test]
    fn safe_svg_rejects_arbitrary_xlink_namespace_alias_external_href() {
        assert!(!safe_svg(br#"<svg xmlns:link="http://www.w3.org/1999/xlink"><a link:href="https://example.com"/></svg>"#));
    }

    #[test]
    fn safe_svg_accepts_alternate_xml_whitespace_around_root() {
        assert!(safe_svg(b"\t\r\n<svg/>\r\n\t"));
    }

    #[test]
    fn safe_svg_accepts_safe_inline_style_fragment_url() {
        assert!(safe_svg(br#"<svg><rect style="fill: url('#id')"/></svg>"#));
    }

    #[test]
    fn safe_svg_rejects_stray_text_outside_root() {
        assert!(!safe_svg(b"text<svg/>"));
        assert!(!safe_svg(b"<svg/>tail"));
    }

    #[test]
    fn safe_svg_rejects_multiple_roots() {
        assert!(!safe_svg(b"<svg/><svg/>"));
    }

    #[test]
    fn safe_css_rejects_single_backslash_escape() {
        assert!(!safe_css_value(r"url(\\68 ttps://example.com)"));
    }

    #[test]
    fn safe_css_rejects_comment_obfuscation() {
        assert!(!safe_css_value("url(/*x*/#safe)"));
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    #[test]
    fn shared_override_is_external_and_confined_to_data_folder() {
        let data = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(data.path().join("logo.png"), b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let manifest = data.path().join("logo-overrides.json");
        let write_map = |relative: &str| {
            let mut projects = std::collections::HashMap::new();
            projects.insert(first.path().to_string_lossy().to_string(), relative);
            projects.insert(second.path().to_string_lossy().to_string(), relative);
            fs::write(
                &manifest,
                serde_json::to_vec(&serde_json::json!({"schemaVersion": 1, "projects": projects}))
                    .unwrap(),
            )
            .unwrap();
        };
        assert_eq!(override_data_url(data.path(), first.path()).unwrap(), None);
        write_map("logo.png");
        let icon = override_data_url(data.path(), first.path())
            .unwrap()
            .unwrap();
        assert!(icon.starts_with("data:image/png;base64,"));
        assert_eq!(
            override_data_url(data.path(), second.path()).unwrap(),
            Some(icon)
        );
        write_map("../logo.png");
        assert!(override_data_url(data.path(), first.path()).is_err());
        write_map("missing.png");
        assert!(override_data_url(data.path(), first.path()).is_err());
        fs::write(data.path().join("logo.png"), b"<svg><script/></svg>").unwrap();
        write_map("logo.png");
        assert!(override_data_url(data.path(), first.path()).is_err());
    }
}
