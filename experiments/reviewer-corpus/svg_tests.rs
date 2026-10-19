#[cfg(test)]
mod corpus_tests {
    use super::*;

    #[test]
    fn invariant_accepts_safe_svg_path() {
        assert!(safe_svg(b"<svg><path d='M0 0'/></svg>"));
    }

    #[test]
    fn invariant_accepts_internal_reference() {
        assert!(safe_svg(b"<svg><use href='#shape'/></svg>"));
    }

    #[test]
    fn contract_rejects_spaced_external_href() {
        assert!(!safe_svg(
            br#"<svg><use href = "https://example.invalid/x"/></svg>"#
        ));
    }

    #[test]
    fn contract_rejects_css_url() {
        assert!(!safe_svg(
            br#"<svg><path fill='url( "https://example.invalid/x" )'/></svg>"#
        ));
    }
}
