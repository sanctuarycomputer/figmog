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

/// Parse a node-id input that may be a bare id or a full Figma URL
/// carrying `node-id=` in its query (spec §2b — the README's promised
/// quick-start prompt pastes a frame URL directly, and every node-id
/// input accepts one). Returns `(file_key, node_id)`: `file_key` is
/// `Some` only when the URL also names a file ([`parse_file_ref`]'s own
/// URL forms — `/design/`, `/file/`, `/board/`); `node_id` is always
/// normalized ([`normalize_node_id`]'s `0-1` ⇒ `0:1` rule).
///
/// A bare (non-URL) `input` always succeeds and passes through
/// unchanged/normalized — "Bare ids: byte-identical behavior to today"
/// (spec §2b). A Figma URL with no `node-id=` in its query can't name a
/// node at all, so that case is `None` rather than silently normalizing
/// the whole URL string as if it were an id.
pub fn parse_node_ref(input: &str) -> Option<(Option<String>, String)> {
    if !input.contains("figma.com/") {
        return Some((None, normalize_node_id(input)));
    }
    let raw_id = query_param(input, "node-id")?;
    let file_key = parse_file_ref(input);
    Some((file_key, normalize_node_id(&raw_id)))
}

/// [`parse_node_ref`] plus a fallback for the cases it can't extract a
/// node id from (a non-Figma-URL string that isn't a bare id shape either
/// — can't happen, [`parse_node_ref`] always succeeds there — or a Figma
/// URL with no `node-id=`): falls back to the raw input, same as
/// [`normalize_node_id`] would do with any unrecognized shape. The file
/// key half is discarded — every `query::*` call site this feeds already
/// operates against one already-resolved store; only `figmog serve`'s
/// multi-file session routing (`serve.rs`) needs the file key, and calls
/// [`parse_node_ref`] directly for that.
pub fn normalize_node_ref(input: &str) -> String {
    parse_node_ref(input)
        .map(|(_, id)| id)
        .unwrap_or_else(|| input.to_string())
}

/// First `key=value` pair named `key` in `url`'s query string (the part
/// after `?`, before any `#` fragment), percent-decoded. `None` if `url`
/// has no query string or no pair named `key`.
fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Minimal `%XX` percent-decoder. Figma node ids in a URL query are ASCII
/// digits/`:`/`-`/`;` (instance paths), so a byte-level decode is
/// sufficient — no `+`-as-space handling, since a query *value* here is
/// never form-encoded space-separated text.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

    #[test]
    fn parse_node_ref_passes_bare_ids_through_normalized() {
        assert_eq!(
            parse_node_ref("0-1"),
            Some((None, "0:1".to_string())),
            "bare id: normalized, no file key"
        );
        assert_eq!(
            parse_node_ref("I206:7;104:22"),
            Some((None, "I206:7;104:22".to_string())),
            "bare instance path: unchanged, byte-identical to normalize_node_id"
        );
    }

    #[test]
    fn parse_node_ref_extracts_node_id_from_a_url() {
        // node-id only, no file segment in this particular URL shape.
        assert_eq!(
            parse_node_ref("https://www.figma.com/board/abc/whatever?node-id=12-345"),
            Some((None, "12:345".to_string()))
        );
    }

    #[test]
    fn parse_node_ref_is_none_for_a_figma_url_with_no_node_id() {
        assert_eq!(
            parse_node_ref("https://www.figma.com/file/flAtUnMfzvA5daBSTFQK35/whatever"),
            None,
            "no node-id in the query: nothing to address, not a bare-id fallback"
        );
    }

    #[test]
    fn parse_node_ref_extracts_both_file_key_and_node_id() {
        assert_eq!(
            parse_node_ref(
                "https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/g3d-Index-Web-Handoff?node-id=0-1&t=x-1"
            ),
            Some((
                Some("flAtUnMfzvA5daBSTFQK35".to_string()),
                "0:1".to_string()
            ))
        );
    }

    #[test]
    fn parse_node_ref_percent_decodes_the_node_id() {
        assert_eq!(
            parse_node_ref("https://www.figma.com/file/abc/x?node-id=1%3A2"),
            Some((None, "1:2".to_string())),
            "1%3A2 decodes to 1:2, already canonical, normalize_node_id leaves it alone"
        );
    }

    #[test]
    fn normalize_node_ref_discards_the_file_key_and_falls_back_when_unresolvable() {
        assert_eq!(normalize_node_ref("0-1"), "0:1");
        assert_eq!(
            normalize_node_ref("https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/x?node-id=1-2"),
            "1:2"
        );
        // A Figma URL with no node-id can't be resolved to an id at all;
        // normalize_node_ref falls back to the raw input rather than
        // erroring, same as normalize_node_id does for any unrecognized
        // shape — the caller's `nodes.get` lookup will simply not find it.
        assert_eq!(
            normalize_node_ref("https://www.figma.com/file/abc/whatever"),
            "https://www.figma.com/file/abc/whatever"
        );
    }
}
