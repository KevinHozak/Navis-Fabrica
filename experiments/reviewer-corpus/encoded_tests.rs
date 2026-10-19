#[cfg(test)]
mod corpus_tests {
    use super::*;

    #[test]
    fn invariant_accepts_safe_path() {
        assert!(safe_svg(b"<svg><path d='M0 0'/></svg>"));
    }

    #[test]
    fn invariant_accepts_internal_aliased_reference() {
        assert!(safe_svg(
            b"<svg xmlns:ref='http://www.w3.org/1999/xlink'><use ref:href='#shape'/></svg>"
        ));
    }

    #[test]
    fn contract_rejects_entity_encoded_external_reference() {
        assert!(!safe_svg(
            br#"<svg><image href="&#x68;ttps://example.invalid/x"/></svg>"#
        ));
    }

    #[test]
    fn contract_rejects_aliased_spaced_external_reference() {
        assert!(!safe_svg(b"<svg xmlns:ref='http://www.w3.org/1999/xlink'><use ref:href = ' https://example.invalid/x'/></svg>"));
    }
}
