//! Version-keyed proxy response cache (spec §12).
//!
//! An upstream `get_*`/`list_*` call whose args carry an explicit node id
//! is cacheable: key it by `hash(tool, canonical args)`, tag the row with
//! the file version it was fetched at, and only serve it back while that
//! version is still current. `store::stale_cache_ids` /
//! `store::evict_stale_cache` handle eviction when the version moves on;
//! this module owns key hashing and the read/write helpers.

use fold::pipeline::terminal::TableReader;
use fold::pipeline::{Keyed, Push};
use fold::stream::{KeyedStream, Readable};
use serde_json::Value;

use crate::model::{Id, ProxyCacheRec, Rec};

/// Deterministic hex key for a `(tool, args_canonical)` pair: FNV-1a 64
/// over `tool`'s bytes, a NUL separator, then `args_canonical`'s bytes.
/// The separator prevents boundary collisions (`tool="ab", args="c"` vs
/// `tool="a", args="bc"` would otherwise hash the same concatenation).
pub fn cache_key(tool: &str, args_canonical: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in tool
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(args_canonical.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// Look up a cached response. A hit requires the stored row's `tool` and
/// `args_canonical` to equal the request's (I-3: FNV-1a 64 is
/// non-cryptographic and trivially collidable, and tool arguments are
/// agent-authored strings — without this check, a colliding key could
/// silently serve one tool's cached response for a different tool/args
/// pair) and its `file_version` to equal `current_version`; a miss (absent,
/// collided, or stale) returns `None` without evicting anything — eviction
/// is a separate, explicit step (see `store::evict_stale_cache`).
pub fn lookup<R: Readable>(
    cache: &TableReader<'_, R, String, ProxyCacheRec>,
    tool: &str,
    args_canonical: &str,
    current_version: &str,
) -> Option<Value> {
    let rec = cache.get(&cache_key(tool, args_canonical))?;
    if rec.tool != tool || rec.args_canonical != args_canonical {
        return None;
    }
    if rec.file_version != current_version {
        return None;
    }
    serde_json::from_str(&rec.content).ok()
}

/// Store (upsert) a response under its cache key, tagged with the file
/// version it was fetched at.
pub fn store<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    tool: &str,
    args_canonical: &str,
    file_version: &str,
    content: &Value,
) {
    let key = cache_key(tool, args_canonical);
    let rec = ProxyCacheRec {
        key_hash: key.clone(),
        tool: tool.to_string(),
        args_canonical: args_canonical.to_string(),
        file_version: file_version.to_string(),
        content: serde_json::to_string(content).unwrap_or_default(),
    };
    st.wtx(|tx| {
        tx.upsert(&Id::ProxyCache(key.clone()), &Rec::ProxyCache(rec.clone()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic() {
        assert_eq!(
            cache_key("get_code", "{\"nodeId\":\"1:2\"}"),
            cache_key("get_code", "{\"nodeId\":\"1:2\"}")
        );
    }

    #[test]
    fn cache_key_distinguishes_boundary_shift() {
        assert_ne!(cache_key("ab", "c"), cache_key("a", "bc"));
    }

    #[test]
    fn cache_key_distinguishes_tool_and_args() {
        assert_ne!(
            cache_key("get_code", "{\"nodeId\":\"1:2\"}"),
            cache_key("get_variable_defs", "{\"nodeId\":\"1:2\"}")
        );
    }

    /// I-3: `lookup` must not trust the key hash alone. Construct a row
    /// directly under the exact key `lookup("get_code", ...)` will compute,
    /// but tagged with a *different* tool (as a real FNV-64 collision
    /// would look, without needing to actually find one) — the row must be
    /// treated as a miss, not served as `get_code`'s cached response.
    #[test]
    fn lookup_misses_on_collision_where_key_matches_but_tool_and_args_dont() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));

        let requested_tool = "get_code";
        let requested_args = "{\"nodeId\":\"1:2\"}";
        let key = cache_key(requested_tool, requested_args);

        // A row that collides on `key_hash` but actually belongs to a
        // different (tool, args) pair — exactly what a deliberate FNV-64
        // collision would produce.
        let colliding_rec = ProxyCacheRec {
            key_hash: key.clone(),
            tool: "get_variable_defs".to_string(),
            args_canonical: "{\"nodeId\":\"9:9\"}".to_string(),
            file_version: "100".to_string(),
            content: serde_json::to_string(&Value::String("wrong tool's data".into())).unwrap(),
        };
        st.wtx(|tx| {
            tx.upsert(
                &Id::ProxyCache(key.clone()),
                &Rec::ProxyCache(colliding_rec),
            );
        });

        let hit = st.rtx(|(_, _, _, _, _, _, _, cache)| {
            lookup(&cache, requested_tool, requested_args, "100")
        });
        assert_eq!(hit, None, "a key-hash collision must never be served");
    }

    /// Sanity complement to the collision test: a row that *does* match
    /// `tool`/`args_canonical` at the same key is still a hit.
    #[test]
    fn lookup_hits_when_tool_and_args_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));

        let tool = "get_code";
        let args = "{\"nodeId\":\"1:2\"}";
        let key = cache_key(tool, args);
        let rec = ProxyCacheRec {
            key_hash: key.clone(),
            tool: tool.to_string(),
            args_canonical: args.to_string(),
            file_version: "100".to_string(),
            content: serde_json::to_string(&Value::String("real data".into())).unwrap(),
        };
        st.wtx(|tx| {
            tx.upsert(&Id::ProxyCache(key.clone()), &Rec::ProxyCache(rec));
        });

        let hit = st.rtx(|(_, _, _, _, _, _, _, cache)| lookup(&cache, tool, args, "100"));
        assert_eq!(hit, Some(Value::String("real data".into())));
    }
}
