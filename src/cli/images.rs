//! `figmog images` (v0.0.2 spec §5): download node renders and/or fill
//! images to `--out`, printing a JSON manifest.
//!
//! **Routing.** Images spends Figma API budget *and* needs to write cache
//! rows into the mirror's store, so it can't safely open the store
//! directly while `figmog serve` holds it (the same lock-contention
//! concern `cli::pull`'s own doc comment documents — unlike every other
//! read command, a running serve doesn't make this "always fresh, no
//! lock" for free, because this command writes too). Socket reachable ⇒
//! route through the very `figmog_images` tool `figmog serve` itself
//! exposes (spec: "the tool path must exist anyway" — serve owns the
//! caching); unreachable, or `--no-socket` ⇒ direct network + direct store
//! open, exactly like `pull`.
//!
//! **Manifest shape**, identical on both paths:
//! `[{id|ref, kind, format, bytes, cached, path?, error?}]` — `bytes` is
//! always a byte *count*; `path` is present only for an item this
//! invocation actually wrote to disk. Exit 0 if at least one item wrote
//! successfully, else 1 — but the manifest is always the stdout payload,
//! even on the all-failed path (spec: "no-token error path produces
//! manifest-shaped error JSON, exit 1" — this command never falls through
//! to the generic `{"error": ...}` shape other commands use).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::api::{FigmaApi, UreqApi};
use crate::images;

use super::{Db, open_store_checked, socket, write_json};

pub(super) fn cmd_images(
    db: &Db,
    no_socket: bool,
    ids: Vec<String>,
    format: String,
    scale: Option<f64>,
    out: Option<PathBuf>,
) -> Result<(), String> {
    let out_dir = out.unwrap_or_else(|| PathBuf::from("figmog-images"));

    let fetched = if !no_socket
        && let Some(result) = socket::try_images_call(
            Path::new(socket::DEFAULT_ROOT),
            json!({"ids": ids, "format": format, "scale": scale, "file": db.key}),
        ) {
        from_socket_result(result?)?
    } else {
        fetch_direct(db, &ids, &format, scale)?
    };

    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("creating {}: {e}", out_dir.display()))?;

    let mut manifest = Vec::with_capacity(fetched.len());
    let mut wrote_any = false;
    for item in fetched {
        let mut entry = item.entry;
        match item.data {
            Some(bytes) => {
                let path = out_dir.join(filename(&entry));
                std::fs::write(&path, &bytes)
                    .map_err(|e| format!("writing {}: {e}", path.display()))?;
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("path".to_string(), json!(path.display().to_string()));
                }
                wrote_any = true;
            }
            None => {
                // No bytes to write: either this item's own `error` field
                // already explains why, or (a socket-routed >1MB item) we
                // synthesize one — the manifest is never silently missing
                // a reason a real file didn't show up.
                if let Some(obj) = entry.as_object_mut()
                    && !obj.contains_key("error")
                {
                    obj.insert(
                        "error".to_string(),
                        json!(
                            "too large to relay over the socket-routed tool call (>1MB) — stop `figmog serve` for this file, or use --no-socket, to fetch it directly"
                        ),
                    );
                }
            }
        }
        manifest.push(entry);
    }

    write_json(&Value::Array(manifest))?;
    if !wrote_any {
        // The manifest above is already on stdout — this is the "exit 1,
        // but still manifest-shaped JSON" path (spec §5), not the generic
        // `{"error": ...}` exit-1 shape `run()` builds from an `Err`. See
        // this module's doc comment.
        std::process::exit(1);
    }
    Ok(())
}

/// One fetched-or-not item, ready to write: `entry` is always the
/// manifest row (built by `images::ImageItem::manifest_entry`, or parsed
/// from a socket-routed tool response's own manifest text block); `data`
/// is the bytes to write to disk when this invocation actually has them.
#[derive(Debug)]
struct Fetched {
    entry: Value,
    data: Option<Vec<u8>>,
}

fn fetch_direct(
    db: &Db,
    ids: &[String],
    format: &str,
    scale: Option<f64>,
) -> Result<Vec<Fetched>, String> {
    let key = db
        .key
        .clone()
        .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
    let node_ids = images::normalize_ids(ids);
    let scale_m = images::scale_milli(scale);
    let mut st = open_store_checked(|| crate::open_store!(&db.path))?;
    let scanned = st.rtx(|((nodes, ..), _, _, _, _, _, meta, _, _, images_table)| {
        images::scan(&nodes, &meta, &images_table, &node_ids, format, scale_m)
    });
    let token = std::env::var("FIGMA_TOKEN").ok();
    let api = token.map(UreqApi::new);
    let download = crate::api::download_bytes;
    let items = images::resolve(
        &mut st,
        scanned,
        api.as_ref().map(|a| a as &dyn FigmaApi),
        &download,
        &key,
        &node_ids,
        format,
        scale,
    );
    Ok(items
        .into_iter()
        .map(|item| {
            let entry = item.manifest_entry();
            let data = item.error.is_none().then_some(item.bytes);
            Fetched { entry, data }
        })
        .collect())
}

/// Decode a socket-routed `figmog_images` tool result (the raw MCP
/// `{"content": [...], "isError": ...}` object — see
/// `socket::try_images_call`'s own doc comment for why this bypasses the
/// generic `interpret_call_response`) back into [`Fetched`] rows: the
/// manifest lives in `content[0]`'s text (always present, always first —
/// `images::to_mcp_content`'s own ordering contract), and every
/// `image`/oversized-`text` block after it carries the same `id`/`ref` tag
/// as its manifest row, matched by that tag rather than by position (an
/// oversized item's block is `text`, not `image`, at whatever position it
/// falls — see `images::to_mcp_content`).
fn from_socket_result(result: Value) -> Result<Vec<Fetched>, String> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let msg = result["content"][0]["text"]
            .as_str()
            .unwrap_or("unknown error from serve")
            .to_string();
        return Err(msg);
    }
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let manifest_text = content
        .first()
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| "serve returned an images result with no manifest block".to_string())?;
    let manifest: Vec<Value> = serde_json::from_str(manifest_text)
        .map_err(|e| format!("serve returned a non-JSON images manifest: {e}"))?;

    Ok(manifest
        .into_iter()
        .map(|entry| {
            let subject = entry.get("id").or_else(|| entry.get("ref"));
            let data = subject.and_then(|subject| {
                content.iter().skip(1).find_map(|block| {
                    if block.get("id").or_else(|| block.get("ref")) != Some(subject) {
                        return None;
                    }
                    // `images::to_mcp_content`'s own contract: a payload
                    // block (image OR svg-as-text) always carries
                    // `mimeType`; an "oversized, use --out" note never
                    // does — that field's presence, not the block's
                    // `type` alone, is what tells the two `text` shapes
                    // apart (see that function's doc comment).
                    block.get("mimeType")?;
                    match block.get("type").and_then(Value::as_str) {
                        Some("image") => block
                            .get("data")
                            .and_then(Value::as_str)
                            .and_then(|b64| images::base64_decode(b64).ok()),
                        Some("text") => block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|s| s.as_bytes().to_vec()),
                        _ => None,
                    }
                })
            });
            Fetched { entry, data }
        })
        .collect())
}

/// `<out>/<kind>-<sanitized subject>.<format>` — node ids/imageRef hashes
/// never contain `/`, but node ids do contain `:` (not a safe bare
/// filename character on every platform figmog might run on), so it's
/// replaced.
fn filename(entry: &Value) -> String {
    let kind = entry["kind"].as_str().unwrap_or("image");
    let subject = entry["id"]
        .as_str()
        .or_else(|| entry["ref"].as_str())
        .unwrap_or("unknown");
    let format = entry["format"]
        .as_str()
        .filter(|f| !f.is_empty())
        .unwrap_or("bin");
    let safe_subject = subject.replace([':', '/', '\\'], "-");
    format!("{kind}-{safe_subject}.{format}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_sanitizes_node_id_colons() {
        let entry = json!({"id": "1:2", "kind": "render", "format": "png"});
        assert_eq!(filename(&entry), "render-1-2.png");
    }

    #[test]
    fn filename_uses_ref_for_fills() {
        let entry = json!({"ref": "hash123", "kind": "fill", "format": "jpg"});
        assert_eq!(filename(&entry), "fill-hash123.jpg");
    }

    #[test]
    fn from_socket_result_matches_image_blocks_by_tag_not_position() {
        // Deliberately out-of-order relative to the manifest, and with an
        // oversized (text-only) second item interleaved, to prove the
        // match is tag-based.
        let result = json!({
            "isError": false,
            "content": [
                {"type": "text", "text": serde_json::to_string(&json!([
                    {"id": "1:2", "kind": "render", "format": "png", "bytes": 3, "cached": false},
                    {"id": "1:3", "kind": "render", "format": "png", "bytes": 2_000_000, "cached": false},
                ])).unwrap()},
                {"type": "text", "id": "1:3", "text": "oversized note"},
                {"type": "image", "id": "1:2", "data": "AQID", "mimeType": "image/png"},
            ],
        });
        let fetched = from_socket_result(result).unwrap();
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].data, Some(vec![1, 2, 3]));
        assert_eq!(fetched[1].data, None);
    }

    /// SVG ruling: an SVG render's payload arrives as a `text` block
    /// (never `image`), distinguished from a plain oversized note by the
    /// `mimeType` field's presence — `from_socket_result` must decode it
    /// back to bytes via the `text` field, not treat it as "no data" just
    /// because its `type` isn't `image`.
    #[test]
    fn from_socket_result_decodes_svg_text_payload_by_mimetype_marker() {
        let svg = "<svg><rect/></svg>";
        let result = json!({
            "isError": false,
            "content": [
                {"type": "text", "text": serde_json::to_string(&json!([
                    {"id": "1:2", "kind": "render", "format": "svg", "bytes": svg.len(), "cached": false},
                ])).unwrap()},
                {"type": "text", "id": "1:2", "text": svg, "mimeType": "image/svg+xml"},
            ],
        });
        let fetched = from_socket_result(result).unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].data, Some(svg.as_bytes().to_vec()));
    }

    /// An oversized-note `text` block (no `mimeType`) must never be
    /// mistaken for an SVG payload just because both are `type: "text"`.
    #[test]
    fn from_socket_result_does_not_mistake_an_oversized_note_for_svg_payload() {
        let result = json!({
            "isError": false,
            "content": [
                {"type": "text", "text": serde_json::to_string(&json!([
                    {"id": "1:2", "kind": "render", "format": "svg", "bytes": 2_000_000, "cached": false},
                ])).unwrap()},
                {"type": "text", "id": "1:2", "text": "too large, use --out"},
            ],
        });
        let fetched = from_socket_result(result).unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].data, None);
    }

    #[test]
    fn from_socket_result_propagates_is_error() {
        let result = json!({
            "isError": true,
            "content": [{"type": "text", "text": "no such file"}],
        });
        assert_eq!(from_socket_result(result).unwrap_err(), "no such file");
    }
}
