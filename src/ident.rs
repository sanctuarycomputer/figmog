//! Parsing of user-supplied file references and node ids.

/// Extract a file key from a bare key or a figma.com URL
/// (`figma.com/design/<key>/…`, `figma.com/file/<key>/…`).
pub fn parse_file_ref(input: &str) -> Option<String> {
    let is_key = |s: &str| s.len() >= 10 && s.chars().all(|c| c.is_ascii_alphanumeric());
    if is_key(input) {
        return Some(input.to_string());
    }
    let rest = input.split_once("figma.com/").map(|(_, r)| r)?;
    let mut parts = rest.split('/');
    match parts.next()? {
        "design" | "file" | "board" => {}
        _ => return None,
    }
    let key = parts.next()?;
    is_key(key).then(|| key.to_string())
}

/// Canonicalize a node id: URLs write `12:34` as `12-34`. Ids that are not
/// exactly `<digits>-<digits>` (already-canonical ids, instance paths like
/// `I206:7;104:22`) pass through unchanged.
pub fn normalize_node_id(input: &str) -> String {
    if let Some((a, b)) = input.split_once('-') {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if digits(a) && digits(b) {
            return format!("{a}:{b}");
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bare_key() {
        assert_eq!(
            parse_file_ref("flAtUnMfzvA5daBSTFQK35").as_deref(),
            Some("flAtUnMfzvA5daBSTFQK35")
        );
    }

    #[test]
    fn parses_design_and_file_urls() {
        for url in [
            "https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/g3d-Index-Web-Handoff?node-id=0-1&t=x-1",
            "https://www.figma.com/file/flAtUnMfzvA5daBSTFQK35/whatever",
            "figma.com/design/flAtUnMfzvA5daBSTFQK35",
        ] {
            assert_eq!(
                parse_file_ref(url).as_deref(),
                Some("flAtUnMfzvA5daBSTFQK35"),
                "{url}"
            );
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_file_ref("https://example.com/nope"), None);
        assert_eq!(parse_file_ref("not a key!"), None);
        assert_eq!(parse_file_ref(""), None);
    }

    #[test]
    fn normalizes_node_ids() {
        assert_eq!(normalize_node_id("0-1"), "0:1");
        assert_eq!(normalize_node_id("12-345"), "12:345");
        assert_eq!(normalize_node_id("12:345"), "12:345");
        // instance sub-node paths pass through untouched
        assert_eq!(normalize_node_id("I206:7;104:22"), "I206:7;104:22");
    }
}
