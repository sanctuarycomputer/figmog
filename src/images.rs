//! Image bytes fetch orchestration (v0.0.2 spec §5): version-cached node
//! renders and fill images, shared by the CLI `figmog images` command
//! (`cli::images`) and `figmog serve`'s `figmog_images` tool
//! (`sessions::FileSession::images`, wired in `serve.rs`).
//!
//! Two fetch kinds behind one call: **renders** (`GET /v1/images/:key`,
//! one node → one image) and **fills** (`GET /v1/files/:key/images`, a
//! file-wide `imageRef` hash → URL map, scanned out of the *requested*
//! nodes' raw `fills` arrays). Both are cached in the `images` table,
//! keyed by [`image_key`] and version-gated exactly like `proxy_cache`
//! (`store::stale_image_ids` / `store::evict_stale_cache`) — a repeat
//! request against an unchanged file spends zero Figma API calls.
//!
//! **Two-step call shape.** [`scan`] (read-only, against already-open
//! table readers) then [`resolve`] (network + cache-write, against
//! `&mut KeyedStream<Id, Rec, P>`) rather than one function that does
//! both: `KeyedStream`'s pipeline type is unnameable outside its own
//! `open_store!` expansion site (see `dispatch.rs`'s and `store.rs`'s own
//! doc comments on the same constraint), so a function generic over
//! `P: Push<..>` can call `st.wtx(..)` (as `resolve` does — writes don't
//! hit this) but can't destructure `st.rtx(..)`'s reader tuple, since that
//! tuple's type is `P`'s own opaque associated type. Every caller
//! therefore calls `st.rtx(|tuple| scan(..))` at its own concrete
//! `open_store!` call site, then passes the result into `resolve` —
//! exactly the shape `store::collect_sweepable` + `store::sync` already
//! use at `cli::pull::do_pull` and `sessions::open_session_at`.
//!
//! `resolve`'s network dependencies are both injected (`api: Option<&dyn
//! FigmaApi>`, `download: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>`) so
//! this module's own tests exercise the full orchestration — cache
//! hit/miss, the fills dedup, per-item 429/error handling — against a
//! scripted fake, with no live network call anywhere in this crate's test
//! suite (documented per the v0.0.2 plan: images has no automated
//! live-network test).

use std::collections::{BTreeMap, BTreeSet};

use fold::pipeline::terminal::TableReader;
use fold::pipeline::{Keyed, Push};
use fold::stream::{KeyedStream, Readable};
use serde_json::{Value, json};

use crate::api::{ApiError, FigmaApi};
use crate::ident::normalize_node_ref;
use crate::model::{FileMeta, Id, ImageBlobRec, NodeRec, Rec};

/// [`ImageItem::kind`] for a node render.
pub const RENDER: &str = "render";
/// [`ImageItem::kind`] for a fill image.
pub const FILL: &str = "fill";

/// A caller with no Figma API token still gets every cache hit for free
/// (spec §5: "a repeat request on an unchanged file spends zero budget");
/// every miss gets this exact per-item manifest error instead of a wasted
/// (guaranteed-401) network attempt.
const NO_TOKEN_MSG: &str = "FIGMA_TOKEN not set — required for image downloads";

/// Content blocks over roughly this many bytes are excluded from the MCP
/// tool's inline content (spec §5's "1MB" cap) — 1 MiB exactly, matching
/// the spec's own "1MB each" wording as closely as a byte count can.
///
/// **Measured on the encoded payload actually shipped, not raw image
/// bytes.** A raster image ships as base64, which inflates the raw byte
/// count by roughly 4/3 (RFC 4648) — a ~786KB PNG becomes a ~1.05MB
/// `data` string, already over this cap even though the *file* is well
/// under 1MB. `to_mcp_content` therefore encodes once, measures the
/// encoded string's length, and compares that (see its own doc comment).
/// SVG ships as raw text (no base64 step — see that function's SVG
/// branch), so for SVG this cap is measured on the markup's own UTF-8
/// byte length directly, which happens to equal the raw byte count.
pub const CONTENT_SIZE_CAP: usize = 1_048_576;

/// Deterministic hex key for one cached image row: FNV-1a 64 (same
/// constants and separator discipline as `cache::cache_key`'s own doc
/// comment, extended from two fields to four) over `kind`+NUL+`subject`+
/// NUL+`format`+NUL+`scale_milli` (decimal) — a boundary shift across any
/// adjacent pair can't collide two distinct cache rows.
///
/// Fills are never format-parameterized (Figma's fills endpoint takes no
/// `format` query — a given `imageRef` always resolves to the same
/// bytes), so every fill lookup/store call passes `format = ""` here
/// regardless of the format actually discovered on download; only render
/// keys vary by the caller's requested format.
pub fn image_key(kind: &str, subject: &str, format: &str, scale_milli: u32) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let scale_str = scale_milli.to_string();
    let mut hash = OFFSET_BASIS;
    for byte in kind
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(subject.as_bytes())
        .chain(std::iter::once(&0u8))
        .chain(format.as_bytes())
        .chain(std::iter::once(&0u8))
        .chain(scale_str.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// `scale` (a caller-facing `f64`, e.g. `1.5`) as the integer milli-scale
/// [`ImageBlobRec::scale_milli`]/[`image_key`] store — `None` (Figma's own
/// default) is `1000` (1×).
pub fn scale_milli(scale: Option<f64>) -> u32 {
    match scale {
        Some(s) => (s * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32,
        None => 1000,
    }
}

/// Normalize and dedup a caller's raw id list — accepts anything
/// `ident::normalize_node_ref` does (bare ids, `0-1` dash form, Figma URLs
/// carrying `node-id=`, spec §2b) — preserving first-seen order. Callers
/// run this once and pass the same `Vec` to both [`scan`] and [`resolve`].
pub fn normalize_ids(ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in ids {
        let id = normalize_node_ref(raw);
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    out
}

/// One fetched (or cache-hit) image: a node render or a fill image, ready
/// for a caller to write to disk and/or wrap as MCP content.
#[derive(Debug, Clone)]
pub struct ImageItem {
    /// Node id (`kind == RENDER`) or `imageRef` hash (`kind == FILL`).
    pub subject: String,
    pub kind: &'static str,
    pub format: String,
    /// Empty when `error.is_some()`.
    pub bytes: Vec<u8>,
    pub cached: bool,
    pub error: Option<String>,
}

impl ImageItem {
    /// This item's manifest row: `{"id"|"ref": subject, "kind", "format",
    /// "bytes": <byte count>, "cached", "error"?}` — `id` for a render,
    /// `ref` for a fill (spec §5: `[{id|ref, kind, path, bytes, cached}]`).
    /// Never carries raw bytes or a `path` — callers (`cli::images`,
    /// `serve.rs`'s tool wrapping) layer those in themselves, since only
    /// the CLI knows about `--out` files at all.
    pub fn manifest_entry(&self) -> Value {
        let mut obj = serde_json::Map::new();
        let subject_key = if self.kind == RENDER { "id" } else { "ref" };
        obj.insert(subject_key.to_string(), json!(self.subject));
        obj.insert("kind".to_string(), json!(self.kind));
        obj.insert("format".to_string(), json!(self.format));
        obj.insert("bytes".to_string(), json!(self.bytes.len()));
        obj.insert("cached".to_string(), json!(self.cached));
        if let Some(e) = &self.error {
            obj.insert("error".to_string(), json!(e));
        }
        Value::Object(obj)
    }
}

/// Everything the read-only half of image fetching can determine from the
/// mirror alone, with no network access: the current file version, every
/// render/fill cache hit, and the set of `imageRef`s the requested nodes'
/// raw `fills` reference. Produced by [`scan`], consumed by [`resolve`] —
/// see this module's doc comment for why they're split at all.
pub struct Scanned {
    version: String,
    render_hits: BTreeMap<String, ImageItem>,
    fill_refs: BTreeSet<String>,
    fill_hits: BTreeMap<String, ImageItem>,
}

/// Whether a row fetched by its key hash actually belongs to the
/// requested `(kind, subject)` pair (and, for a render, the requested
/// `format`/`scale_milli` too) — the same "never trust the key hash
/// alone" discipline `cache::lookup` documents (I-3 there): a
/// non-cryptographic FNV-64 hash is trivially collidable, and a colliding
/// row must read as a miss, not get served as a different item's bytes.
/// Fills skip the format/scale check on purpose: [`image_key`] always
/// keys a fill's row with `format = ""`/`scale_milli = 1000` regardless
/// of the format actually discovered on download (see that function's
/// own doc comment), so checking the *stored* `format` there would reject
/// every legitimate fill hit, not just collisions.
fn matches_identity(
    rec: &ImageBlobRec,
    kind: &str,
    subject: &str,
    format: &str,
    scale_milli: u32,
) -> bool {
    if rec.kind != kind || rec.subject != subject {
        return false;
    }
    kind != RENDER || (rec.format == format && rec.scale_milli == scale_milli)
}

/// Read-only half of image fetching: cache lookups plus fill-ref
/// detection, against already-open table readers — call inside
/// `st.rtx(|tuple| images::scan(..))` at a concrete `open_store!` site.
/// `node_ids` must already be normalized/deduped ([`normalize_ids`]).
pub fn scan<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
    images: &TableReader<'_, R, String, ImageBlobRec>,
    node_ids: &[String],
    format: &str,
    scale_milli: u32,
) -> Scanned {
    let version = meta.get(&0).map(|m| m.version.clone()).unwrap_or_default();

    let mut render_hits: BTreeMap<String, ImageItem> = BTreeMap::new();
    for id in node_ids {
        let key = image_key(RENDER, id, format, scale_milli);
        if let Some(rec) = images.get(&key)
            && rec.file_version == version
            && matches_identity(&rec, RENDER, id, format, scale_milli)
        {
            render_hits.insert(
                id.clone(),
                ImageItem {
                    subject: id.clone(),
                    kind: RENDER,
                    format: format.to_string(),
                    bytes: rec.bytes.clone(),
                    cached: true,
                    error: None,
                },
            );
        }
    }

    let mut fill_refs: BTreeSet<String> = BTreeSet::new();
    for id in node_ids {
        if let Some(n) = nodes.get(id)
            && let Ok(raw) = serde_json::from_str::<Value>(&n.raw)
        {
            fill_refs.extend(extract_image_refs(&raw));
        }
    }

    let mut fill_hits: BTreeMap<String, ImageItem> = BTreeMap::new();
    for r in &fill_refs {
        let key = image_key(FILL, r, "", 1000);
        if let Some(rec) = images.get(&key)
            && rec.file_version == version
            && matches_identity(&rec, FILL, r, "", 1000)
        {
            fill_hits.insert(
                r.clone(),
                ImageItem {
                    subject: r.clone(),
                    kind: FILL,
                    format: rec.format.clone(),
                    bytes: rec.bytes.clone(),
                    cached: true,
                    error: None,
                },
            );
        }
    }

    Scanned {
        version,
        render_hits,
        fill_refs,
        fill_hits,
    }
}

/// Network + cache-write half of image fetching: fetch every render/fill
/// miss `scan` didn't already resolve — at most one `images_render` call
/// covering every render miss, at most one `file_image_fills` call
/// covering every distinct `imageRef` miss (spec §5) — cache the results,
/// and return every item in `node_ids`' request order followed by the
/// scanned fill refs' sorted order. `api: None` means "no Figma API token
/// available": cache hits still resolve fully; every miss gets a
/// `"FIGMA_TOKEN not set"` error instead of a network attempt.
#[allow(clippy::too_many_arguments)]
pub fn resolve<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    scanned: Scanned,
    api: Option<&dyn FigmaApi>,
    download: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>,
    file_key: &str,
    node_ids: &[String],
    format: &str,
    scale: Option<f64>,
) -> Vec<ImageItem> {
    let Scanned {
        version,
        mut render_hits,
        fill_refs,
        mut fill_hits,
    } = scanned;
    let scale_m = scale_milli(scale);

    let render_misses: Vec<String> = node_ids
        .iter()
        .filter(|id| !render_hits.contains_key(*id))
        .cloned()
        .collect();
    if !render_misses.is_empty() {
        match api {
            None => {
                for id in &render_misses {
                    render_hits.insert(id.clone(), no_token_item(RENDER, id, format));
                }
            }
            Some(api) => match api.images_render(file_key, &render_misses, format, scale) {
                Ok(resp) => {
                    let url_map = resp.get("images").and_then(Value::as_object);
                    for id in &render_misses {
                        let url = url_map.and_then(|m| m.get(id)).and_then(Value::as_str);
                        let item = match url {
                            None => error_item(
                                RENDER,
                                id,
                                format.to_string(),
                                "no render available for this node".to_string(),
                            ),
                            Some(url) => match download(url) {
                                Ok(bytes) => {
                                    let key = image_key(RENDER, id, format, scale_m);
                                    store_image(
                                        st, &key, RENDER, id, format, scale_m, &version, &bytes,
                                    );
                                    ImageItem {
                                        subject: id.clone(),
                                        kind: RENDER,
                                        format: format.to_string(),
                                        bytes,
                                        cached: false,
                                        error: None,
                                    }
                                }
                                Err(e) => error_item(RENDER, id, format.to_string(), e.to_string()),
                            },
                        };
                        render_hits.insert(id.clone(), item);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    for id in &render_misses {
                        render_hits.insert(
                            id.clone(),
                            error_item(RENDER, id, format.to_string(), msg.clone()),
                        );
                    }
                }
            },
        }
    }

    let fill_misses: Vec<String> = fill_refs
        .iter()
        .filter(|r| !fill_hits.contains_key(*r))
        .cloned()
        .collect();
    if !fill_misses.is_empty() {
        match api {
            None => {
                for r in &fill_misses {
                    fill_hits.insert(r.clone(), no_token_item(FILL, r, ""));
                }
            }
            Some(api) => match api.file_image_fills(file_key) {
                Ok(resp) => {
                    // Figma's own response nests under `meta.images`; tolerate
                    // a bare `images` map too (this crate's own test fixtures
                    // don't need to reproduce the wrapper exactly).
                    let url_map = resp
                        .get("meta")
                        .and_then(|m| m.get("images"))
                        .and_then(Value::as_object)
                        .or_else(|| resp.get("images").and_then(Value::as_object));
                    for r in &fill_misses {
                        let url = url_map.and_then(|m| m.get(r)).and_then(Value::as_str);
                        let item = match url {
                            None => error_item(
                                FILL,
                                r,
                                String::new(),
                                "no fill image available for this imageRef".to_string(),
                            ),
                            Some(url) => match download(url) {
                                Ok(bytes) => {
                                    let fmt = infer_fill_format(url);
                                    let key = image_key(FILL, r, "", 1000);
                                    store_image(st, &key, FILL, r, &fmt, 1000, &version, &bytes);
                                    ImageItem {
                                        subject: r.clone(),
                                        kind: FILL,
                                        format: fmt,
                                        bytes,
                                        cached: false,
                                        error: None,
                                    }
                                }
                                Err(e) => error_item(FILL, r, String::new(), e.to_string()),
                            },
                        };
                        fill_hits.insert(r.clone(), item);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    for r in &fill_misses {
                        fill_hits
                            .insert(r.clone(), error_item(FILL, r, String::new(), msg.clone()));
                    }
                }
            },
        }
    }

    let mut out = Vec::with_capacity(node_ids.len() + fill_refs.len());
    for id in node_ids {
        if let Some(item) = render_hits.remove(id) {
            out.push(item);
        }
    }
    for r in &fill_refs {
        if let Some(item) = fill_hits.remove(r) {
            out.push(item);
        }
    }
    out
}

fn no_token_item(kind: &'static str, subject: &str, format: &str) -> ImageItem {
    ImageItem {
        subject: subject.to_string(),
        kind,
        format: format.to_string(),
        bytes: Vec::new(),
        cached: false,
        error: Some(NO_TOKEN_MSG.to_string()),
    }
}

fn error_item(kind: &'static str, subject: &str, format: String, error: String) -> ImageItem {
    ImageItem {
        subject: subject.to_string(),
        kind,
        format,
        bytes: Vec::new(),
        cached: false,
        error: Some(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn store_image<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    key: &str,
    kind: &str,
    subject: &str,
    format: &str,
    scale_milli: u32,
    file_version: &str,
    bytes: &[u8],
) {
    let rec = ImageBlobRec {
        key_hash: key.to_string(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        format: format.to_string(),
        scale_milli,
        file_version: file_version.to_string(),
        bytes: bytes.to_vec(),
    };
    st.wtx(|tx| {
        tx.upsert(
            &Id::ImageBlob(key.to_string()),
            &Rec::ImageBlob(rec.clone()),
        );
    });
}

/// Distinct `imageRef` hashes among a raw node JSON's `fills` array
/// (`fills[].type == "IMAGE"`, `fills[].imageRef`) — the fill-detection
/// half of spec §5. Any other fill type (`SOLID`, `GRADIENT_*`) or a
/// missing/non-array `fills` yields nothing.
fn extract_image_refs(raw: &Value) -> Vec<String> {
    let Some(fills) = raw.get("fills").and_then(Value::as_array) else {
        return Vec::new();
    };
    fills
        .iter()
        .filter(|f| f.get("type").and_then(Value::as_str) == Some("IMAGE"))
        .filter_map(|f| f.get("imageRef").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Best-effort format for a fill's bytes, from its download URL's file
/// extension (Figma's fills endpoint carries no format of its own — see
/// [`image_key`]'s doc comment) — informational only, defaulting to
/// `"png"` when the URL has no recognizable extension.
fn infer_fill_format(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    if let Some(idx) = path.rfind('.') {
        let ext = &path[idx + 1..];
        if !ext.is_empty() && ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext.to_ascii_lowercase();
        }
    }
    "png".to_string()
}

fn mime_for(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

/// Build the MCP `tools/call` result for `figmog_images` (spec §5):
/// content is always `[manifest text block, per-item blocks...]` — the
/// manifest first and always text, so a socket-routed CLI client (see
/// `cli::images`) can find it at `content[0]` unambiguously regardless of
/// how many (if any) data-bearing blocks follow. Per successful item, in
/// the same order as `items`:
///
/// - **SVG** (`item.format` case-insensitively `"svg"`) ships as a
///   **`text`** block carrying the raw markup verbatim — never base64 nor
///   `image/svg+xml` (spec ruling): raw SVG text is more directly useful
///   to an agent generating HTML/CSS than an opaque image blob, and MCP
///   clients render `image/svg+xml` image blocks inconsistently. The
///   block also carries a `mimeType: "image/svg+xml"` field (informational
///   only — MCP's `text` content type doesn't define one) so a consumer
///   can tell an actual-SVG-content block apart from an oversized note
///   below without guessing from prose.
/// - **Every other format** ships as an `image` block, base64-encoded.
///
/// Either way, over [`CONTENT_SIZE_CAP`] (measured on the *encoded*
/// payload — the block's own `text`/`data` string — not the raw byte
/// count; see that constant's doc comment) gets a plain text note instead
/// (no `mimeType`, the marker above's absence is what makes it
/// distinguishable from a real SVG block), pointing at `figmog images
/// --out`. An error item gets neither — its manifest entry already
/// carries the reason.
pub fn to_mcp_content(items: &[ImageItem]) -> Value {
    let manifest: Vec<Value> = items.iter().map(ImageItem::manifest_entry).collect();
    let mut content = vec![json!({
        "type": "text",
        "text": serde_json::to_string(&manifest).unwrap_or_default(),
    })];
    for item in items {
        if item.error.is_some() {
            continue;
        }
        let subject_key = if item.kind == RENDER { "id" } else { "ref" };
        let is_svg = item.format.eq_ignore_ascii_case("svg");

        // Non-standard extra field(s) on every arm below, ignored by MCP
        // clients that don't know them — let a socket-routed CLI caller
        // (`cli::images`, the only consumer that reads them) match a
        // block back to its manifest row, and tell payload from note
        // apart, without relying on content order alone (an oversized
        // item's block sits at the same position a payload block would).
        let mut block = if is_svg {
            let text = String::from_utf8_lossy(&item.bytes).into_owned();
            if text.len() <= CONTENT_SIZE_CAP {
                serde_json::Map::from_iter([
                    ("type".to_string(), json!("text")),
                    ("text".to_string(), json!(text)),
                    ("mimeType".to_string(), json!(mime_for(&item.format))),
                ])
            } else {
                oversized_note(item, subject_key, text.len())
            }
        } else {
            let encoded = base64_encode(&item.bytes);
            if encoded.len() <= CONTENT_SIZE_CAP {
                serde_json::Map::from_iter([
                    ("type".to_string(), json!("image")),
                    ("data".to_string(), json!(encoded)),
                    ("mimeType".to_string(), json!(mime_for(&item.format))),
                ])
            } else {
                oversized_note(item, subject_key, encoded.len())
            }
        };
        block.insert(subject_key.to_string(), json!(item.subject));
        content.push(Value::Object(block));
    }
    json!({"content": content, "isError": false})
}

/// The "too large to inline" text block for one item — deliberately never
/// carries a `mimeType` field, the signal [`to_mcp_content`]'s consumers
/// use to tell "this text block IS the payload" (an SVG success) from
/// "this text block is a note" (this function's output).
/// `encoded_len` is whatever [`to_mcp_content`] actually measured against
/// [`CONTENT_SIZE_CAP`] (an encoded byte count, not necessarily
/// `item.bytes.len()` — see that function's own doc comment).
fn oversized_note(
    item: &ImageItem,
    subject_key: &str,
    encoded_len: usize,
) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([
        ("type".to_string(), json!("text")),
        (
            "text".to_string(),
            json!(format!(
                "{} {}={} is {encoded_len} bytes encoded, over the 1MB inline cap — run `figmog images {} --out <dir>` to download it to disk.",
                item.kind, subject_key, item.subject, item.subject
            )),
        ),
    ])
}

// ---- base64 (no crate dependency — see `api::download_bytes`'s doc
// comment for why the MCP wire format needs base64 at all) ----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok((b - b'A') as u32),
            b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte: {b}")),
        }
    }
    let s = s.trim();
    if !s.len().is_multiple_of(4) {
        return Err("base64 input length must be a multiple of 4".to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        let mut n: u32 = 0;
        for &b in chunk {
            n <<= 6;
            if b != b'=' {
                n |= val(b)?;
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crate::api::FileMetaResp;
    use crate::model::NodeRec;

    // ---- image_key ----

    #[test]
    fn image_key_is_deterministic() {
        assert_eq!(
            image_key(RENDER, "1:2", "png", 1000),
            image_key(RENDER, "1:2", "png", 1000)
        );
    }

    #[test]
    fn image_key_distinguishes_kind_subject_format_and_scale() {
        let base = image_key(RENDER, "1:2", "png", 1000);
        assert_ne!(base, image_key(FILL, "1:2", "png", 1000));
        assert_ne!(base, image_key(RENDER, "1:3", "png", 1000));
        assert_ne!(base, image_key(RENDER, "1:2", "svg", 1000));
        assert_ne!(base, image_key(RENDER, "1:2", "png", 2000));
    }

    #[test]
    fn image_key_distinguishes_boundary_shift() {
        // Same separator-discipline proof as `cache::cache_key`'s own test.
        assert_ne!(
            image_key("ab", "c", "", 1000),
            image_key("a", "bc", "", 1000)
        );
    }

    #[test]
    fn scale_milli_defaults_to_1000_and_rounds() {
        assert_eq!(scale_milli(None), 1000);
        assert_eq!(scale_milli(Some(1.0)), 1000);
        assert_eq!(scale_milli(Some(1.5)), 1500);
        assert_eq!(scale_milli(Some(0.333)), 333);
    }

    #[test]
    fn normalize_ids_dedups_and_normalizes_dash_form() {
        let ids = vec!["0-1".to_string(), "0:1".to_string(), "1-2".to_string()];
        assert_eq!(
            normalize_ids(&ids),
            vec!["0:1".to_string(), "1:2".to_string()]
        );
    }

    // ---- extract_image_refs / infer_fill_format ----

    #[test]
    fn extract_image_refs_finds_image_fills_and_ignores_others() {
        let raw = json!({
            "fills": [
                {"type": "SOLID", "color": {"r": 1}},
                {"type": "IMAGE", "imageRef": "abc123"},
                {"type": "IMAGE", "imageRef": "def456"},
            ]
        });
        let mut refs = extract_image_refs(&raw);
        refs.sort();
        assert_eq!(refs, vec!["abc123".to_string(), "def456".to_string()]);
    }

    #[test]
    fn extract_image_refs_empty_when_no_fills() {
        assert_eq!(extract_image_refs(&json!({})), Vec::<String>::new());
    }

    #[test]
    fn infer_fill_format_reads_the_url_extension() {
        assert_eq!(infer_fill_format("https://s3/x/y.jpg?sig=1"), "jpg");
        assert_eq!(infer_fill_format("https://s3/x/y.PNG"), "png");
        assert_eq!(infer_fill_format("https://s3/x/y"), "png");
    }

    // ---- base64 ----

    #[test]
    fn base64_round_trips_classic_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), encoded, "encode {raw:?}");
            assert_eq!(
                base64_decode(encoded).unwrap(),
                raw.as_bytes().to_vec(),
                "decode {encoded:?}"
            );
        }
    }

    // ---- manifest_entry / to_mcp_content ----

    #[test]
    fn manifest_entry_uses_id_for_render_and_ref_for_fill() {
        let render = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![1, 2, 3],
            cached: false,
            error: None,
        };
        assert_eq!(
            render.manifest_entry(),
            json!({"id": "1:2", "kind": "render", "format": "png", "bytes": 3, "cached": false})
        );

        let fill = ImageItem {
            subject: "abc".into(),
            kind: FILL,
            format: "jpg".into(),
            bytes: vec![],
            cached: true,
            error: Some("boom".into()),
        };
        assert_eq!(
            fill.manifest_entry(),
            json!({"ref": "abc", "kind": "fill", "format": "jpg", "bytes": 0, "cached": true, "error": "boom"})
        );
    }

    #[test]
    fn to_mcp_content_orders_manifest_first_then_images_skipping_errors_and_oversized() {
        let ok = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![1, 2, 3],
            cached: false,
            error: None,
        };
        let failed = ImageItem {
            subject: "1:3".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![],
            cached: false,
            error: Some("rate limited".into()),
        };
        let oversized = ImageItem {
            subject: "1:4".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![0u8; CONTENT_SIZE_CAP + 1],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[ok.clone(), failed, oversized]);
        let content = out["content"].as_array().unwrap();
        // manifest first, always text
        assert_eq!(content[0]["type"], json!("text"));
        let manifest: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 3);
        // one image block for the successful, under-cap item, tagged with
        // its node id so a socket-routed CLI caller can match it back up.
        assert_eq!(content[1]["type"], json!("image"));
        assert_eq!(content[1]["mimeType"], json!("image/png"));
        assert_eq!(content[1]["data"], json!(base64_encode(&ok.bytes)));
        assert_eq!(content[1]["id"], json!("1:2"));
        // one text note for the oversized item, also tagged; nothing at
        // all for the error item.
        assert_eq!(content[2]["type"], json!("text"));
        assert!(content[2]["text"].as_str().unwrap().contains("--out"));
        assert_eq!(content[2]["id"], json!("1:4"));
        assert_eq!(content.len(), 3);
        assert_eq!(out["isError"], json!(false));
    }

    /// I2: the cap must bound the *encoded* payload, not the raw byte
    /// count — 800,000 raw bytes is comfortably under
    /// `CONTENT_SIZE_CAP` (1,048,576), but base64's ~4/3 inflation (RFC
    /// 4648) pushes the actual `data` string this item would ship as over
    /// it, so it must still fall back to the oversized note.
    #[test]
    fn to_mcp_content_caps_on_encoded_length_not_raw_bytes() {
        let raw_len = 800_000;
        assert!(raw_len <= CONTENT_SIZE_CAP, "sanity: raw is under the cap");
        let encoded_len = base64_encode(&vec![0u8; raw_len]).len();
        assert!(
            encoded_len > CONTENT_SIZE_CAP,
            "sanity: encoded crosses the cap"
        );

        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![0u8; raw_len],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[1]["type"],
            json!("text"),
            "must fall back to the oversized note, not an image block, once the encoded size crosses the cap"
        );
        assert!(
            content[1]["mimeType"].is_null(),
            "an oversized note must never carry mimeType — that field is the payload marker"
        );
    }

    /// SVG ruling: an SVG render ships as raw markup in a `text` block
    /// (never base64/`image/svg+xml`), tagged with `mimeType:
    /// "image/svg+xml"` so a consumer can tell it apart from an oversized
    /// note (also `text`, but with no `mimeType`).
    #[test]
    fn to_mcp_content_ships_svg_as_raw_text_markup_not_base64_image() {
        let svg = "<svg><rect/></svg>";
        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "svg".into(),
            bytes: svg.as_bytes().to_vec(),
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], json!("text"));
        assert_eq!(content[1]["text"], json!(svg));
        assert_eq!(content[1]["mimeType"], json!("image/svg+xml"));
        assert_eq!(content[1]["id"], json!("1:2"));
    }

    /// SVG ruling, size-cap half: the cap still applies to SVG text, same
    /// encoded-length comparison — for SVG there's no base64 step, so
    /// "encoded length" is just the markup's own UTF-8 byte length.
    #[test]
    fn to_mcp_content_oversized_svg_falls_back_to_a_note_without_mimetype() {
        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "svg".into(),
            bytes: vec![b'a'; CONTENT_SIZE_CAP + 1],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], json!("text"));
        assert!(
            content[1]["mimeType"].is_null(),
            "an oversized SVG note must not carry mimeType either"
        );
        assert!(content[1]["text"].as_str().unwrap().contains("--out"));
    }

    // ---- scan + resolve: scripted FigmaApi fake ----

    /// Pops one scripted response per call (same pattern as
    /// `watch::tests::Script`), and records every call it received so
    /// tests can assert on dedup/call counts.
    struct Script {
        render_responses: RefCell<Vec<Result<Value, ApiError>>>,
        fills_responses: RefCell<Vec<Result<Value, ApiError>>>,
        render_calls: RefCell<Vec<Vec<String>>>,
        fills_calls: RefCell<u32>,
    }
    impl Script {
        fn new() -> Self {
            Script {
                render_responses: RefCell::new(Vec::new()),
                fills_responses: RefCell::new(Vec::new()),
                render_calls: RefCell::new(Vec::new()),
                fills_calls: RefCell::new(0),
            }
        }
    }
    impl FigmaApi for Script {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            unimplemented!("not exercised by images::resolve")
        }
        fn file(&self, _key: &str, _geometry: bool) -> Result<Value, ApiError> {
            unimplemented!("not exercised by images::resolve")
        }
        fn images_render(
            &self,
            _key: &str,
            ids: &[String],
            _format: &str,
            _scale: Option<f64>,
        ) -> Result<Value, ApiError> {
            self.render_calls.borrow_mut().push(ids.to_vec());
            self.render_responses.borrow_mut().remove(0)
        }
        fn file_image_fills(&self, _key: &str) -> Result<Value, ApiError> {
            *self.fills_calls.borrow_mut() += 1;
            self.fills_responses.borrow_mut().remove(0)
        }
    }

    #[allow(clippy::type_complexity)]
    fn downloader(
        bytes: HashMap<String, Vec<u8>>,
    ) -> (
        impl Fn(&str) -> Result<Vec<u8>, ApiError>,
        std::rc::Rc<RefCell<u32>>,
    ) {
        let calls = std::rc::Rc::new(RefCell::new(0u32));
        let calls2 = calls.clone();
        let f = move |url: &str| -> Result<Vec<u8>, ApiError> {
            *calls2.borrow_mut() += 1;
            bytes
                .get(url)
                .cloned()
                .ok_or_else(|| ApiError::Network(format!("no fixture bytes for {url}")))
        };
        (f, calls)
    }

    fn insert_node<P: Push<Keyed<Id, Rec>>>(
        st: &mut KeyedStream<Id, Rec, P>,
        node_id: &str,
        raw: Value,
    ) {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Node(node_id.to_string()),
                &Rec::Node(NodeRec {
                    id: node_id.to_string(),
                    parent_id: Some("0:1".into()),
                    child_index: 0,
                    page_id: "0:1".into(),
                    node_type: "RECTANGLE".into(),
                    name: "N".into(),
                    visible: true,
                    text: None,
                    component_id: None,
                    component_properties: vec![],
                    property_definitions: None,
                    style_refs: vec![],
                    bound_variables: vec![],
                    abs_bounds: None,
                    raw: raw.to_string(),
                }),
            );
        });
    }

    fn insert_meta<P: Push<Keyed<Id, Rec>>>(st: &mut KeyedStream<Id, Rec, P>, version: &str) {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Meta,
                &Rec::Meta(crate::model::FileMeta {
                    name: "F".into(),
                    version: version.to_string(),
                    last_modified: "t".into(),
                    synced_at_unix_ms: 0,
                }),
            );
        });
    }

    // ---- I-3-style collision safety (mirrors cache.rs's own test) ----

    /// A row that collides on `image_key`'s hash but actually belongs to a
    /// different `(kind, subject)` pair must read as a miss, not get
    /// served as a different item's bytes — exactly the guarantee
    /// `cache::lookup`'s own collision test proves for `proxy_cache`.
    #[test]
    fn scan_misses_on_collision_where_key_matches_but_kind_and_subject_dont() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_meta(&mut st, "100");

        let requested_id = "1:2".to_string();
        let key = image_key(RENDER, &requested_id, "png", 1000);
        // Constructed directly under the exact key a render lookup for
        // `requested_id` will compute, but tagged as a *fill* for a
        // different subject — exactly what a real FNV-64 collision would
        // look like, without needing to actually find one.
        let colliding = ImageBlobRec {
            key_hash: key.clone(),
            kind: FILL.to_string(),
            subject: "some-other-ref".to_string(),
            format: "jpg".to_string(),
            scale_milli: 1000,
            file_version: "100".to_string(),
            bytes: vec![9, 9, 9],
        };
        st.wtx(|tx| {
            tx.upsert(&Id::ImageBlob(key), &Rec::ImageBlob(colliding));
        });

        let ids = vec![requested_id.clone()];
        let scanned = st.rtx(|((nodes, ..), _, _, _, _, _, meta, _, _, images)| {
            scan(&nodes, &meta, &images, &ids, "png", 1000)
        });
        assert!(
            !scanned.render_hits.contains_key(&requested_id),
            "a key-hash collision must never be served as a cache hit"
        );
    }

    /// Same collision guarantee, the render-specific half: a row under the
    /// right key with the *right* kind/subject but a different requested
    /// format or scale must still read as a miss — `image_key` folds
    /// format/scale into the hash, but a real collision could still land
    /// two distinct `(format, scale)` requests on the same bucket.
    #[test]
    fn scan_render_hit_requires_matching_format_and_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_meta(&mut st, "100");

        let requested_id = "1:2".to_string();
        let key = image_key(RENDER, &requested_id, "png", 1000);
        let colliding = ImageBlobRec {
            key_hash: key.clone(),
            kind: RENDER.to_string(),
            subject: requested_id.clone(),
            format: "svg".to_string(), // requested "png"
            scale_milli: 2000,         // requested 1000
            file_version: "100".to_string(),
            bytes: vec![9, 9, 9],
        };
        st.wtx(|tx| {
            tx.upsert(&Id::ImageBlob(key), &Rec::ImageBlob(colliding));
        });

        let ids = vec![requested_id.clone()];
        let scanned = st.rtx(|((nodes, ..), _, _, _, _, _, meta, _, _, images)| {
            scan(&nodes, &meta, &images, &ids, "png", 1000)
        });
        assert!(
            !scanned.render_hits.contains_key(&requested_id),
            "a format/scale mismatch under the same key must never be served"
        );
    }

    /// `scan` then `resolve`, exactly the two-step call every production
    /// call site makes (this module's own doc comment) — the one seam
    /// this test suite proves end to end.
    macro_rules! scan_and_resolve {
        ($st:expr, $api:expr, $download:expr, $ids:expr, $format:expr, $scale:expr) => {{
            let scanned = $st.rtx(|((nodes, ..), _, _, _, _, _, meta, _, _, images)| {
                scan(&nodes, &meta, &images, $ids, $format, scale_milli($scale))
            });
            resolve(
                &mut $st, scanned, $api, $download, "FILE", $ids, $format, $scale,
            )
        }};
    }

    #[test]
    fn resolve_render_miss_downloads_and_caches_then_a_repeat_call_hits_cache_with_zero_api_calls()
    {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        let (download, download_calls) = downloader(HashMap::from([(
            "https://s3/render.png".to_string(),
            vec![1, 2, 3, 4],
        )]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].bytes, vec![1, 2, 3, 4]);
        assert!(!items[0].cached);
        assert!(items[0].error.is_none());
        assert_eq!(*download_calls.borrow(), 1);
        assert_eq!(script.render_calls.borrow().len(), 1);

        // Second call: cache hit. `Script`'s render_responses is now empty,
        // so a second `images_render` call would panic on `.remove(0)` —
        // proving zero API calls happened, not just asserting a counter.
        let items2 = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].bytes, vec![1, 2, 3, 4]);
        assert!(items2[0].cached);
        assert_eq!(
            *download_calls.borrow(),
            1,
            "cache hit must not re-download"
        );
    }

    #[test]
    fn resolve_detects_fill_image_refs_and_fetches_the_map_at_most_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(
            &mut st,
            "1:2",
            json!({"fills": [{"type": "IMAGE", "imageRef": "hash1"}]}),
        );
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        script.fills_responses.borrow_mut().push(Ok(
            json!({"meta": {"images": {"hash1": "https://s3/fill.jpg"}}}),
        ));
        let (download, _calls) = downloader(HashMap::from([
            ("https://s3/render.png".to_string(), vec![9]),
            ("https://s3/fill.jpg".to_string(), vec![5, 5, 5]),
        ]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 2, "one render + one fill");
        let fill = items.iter().find(|i| i.kind == FILL).unwrap();
        assert_eq!(fill.subject, "hash1");
        assert_eq!(fill.bytes, vec![5, 5, 5]);
        assert_eq!(fill.format, "jpg");
        assert_eq!(*script.fills_calls.borrow(), 1);

        // Cache hit path for the fill: `fills_responses` is now empty, so a
        // second call would panic if the fill map were re-fetched.
        let items2 = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        let fill2 = items2.iter().find(|i| i.kind == FILL).unwrap();
        assert!(fill2.cached);
        assert_eq!(*script.fills_calls.borrow(), 1);
    }

    #[test]
    fn resolve_records_a_missing_render_url_as_a_per_item_error_without_failing_other_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_node(&mut st, "1:3", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        // Figma omits 1:3 from the map — not renderable.
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        let (download, _calls) = downloader(HashMap::from([(
            "https://s3/render.png".to_string(),
            vec![7],
        )]));

        let ids = vec!["1:2".to_string(), "1:3".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 2);
        let ok = items.iter().find(|i| i.subject == "1:2").unwrap();
        assert!(ok.error.is_none());
        assert_eq!(ok.bytes, vec![7]);
        let missing = items.iter().find(|i| i.subject == "1:3").unwrap();
        assert!(missing.error.is_some());
        assert!(missing.bytes.is_empty());
    }

    #[test]
    fn resolve_without_api_serves_cache_hits_and_marks_misses_with_the_no_token_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_node(&mut st, "1:3", json!({}));
        insert_meta(&mut st, "100");
        // Pre-seed a cache hit for 1:2 directly (as if fetched earlier).
        {
            let key = image_key(RENDER, "1:2", "png", 1000);
            store_image(&mut st, &key, RENDER, "1:2", "png", 1000, "100", &[1, 1]);
        }
        let (download, download_calls) = downloader(HashMap::new());

        let ids = vec!["1:2".to_string(), "1:3".to_string()];
        let items = scan_and_resolve!(st, None, &download, &ids, "png", None);
        assert_eq!(items.len(), 2);
        let cached = items.iter().find(|i| i.subject == "1:2").unwrap();
        assert!(cached.cached);
        assert_eq!(cached.bytes, vec![1, 1]);
        let miss = items.iter().find(|i| i.subject == "1:3").unwrap();
        assert_eq!(miss.error.as_deref(), Some(NO_TOKEN_MSG));
        assert_eq!(*download_calls.borrow(), 0);
    }

    #[test]
    fn resolve_records_a_429_uniformly_across_every_render_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Err(ApiError::RateLimited {
                retry_after: std::time::Duration::from_secs(30),
            }));
        let (download, _calls) = downloader(HashMap::new());
        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 1);
        let err = items[0].error.as_deref().unwrap();
        assert!(err.contains("rate limited"), "{err}");
    }
}
