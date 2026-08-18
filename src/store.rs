//! Pipeline definition and the sync transaction.
//!
//! The pipeline type contains fn items and so can't be written down; the
//! `figmog_pipeline!` / `open_store!` macros expand it at each use site
//! (main + tests). Everything else here is ordinary generic functions.

use std::collections::BTreeSet;

use fold::pipeline::{Keyed, Push};
use fold::stream::KeyedStream;
use serde::Serialize;

use crate::flatten::Flattened;
use crate::model::{FileMeta, Id, MirrorConfigRec, NodeRec, ProxyCacheRec, Rec};

// ---- pipeline branch functions (pure; fold requires determinism) ----

/// Feeds the `nodes` table: keep only `Rec::Node` records, keyed by node id.
pub fn node_only(d: &Keyed<Id, Rec>) -> Option<Keyed<String, NodeRec>> {
    match &d.val {
        Rec::Node(n) => Some(Keyed::new(n.id.clone(), n.clone())),
        _ => None,
    }
}

/// Feeds the `children` multimap: parent id -> (child_index, child id).
/// The document root has no `parent_id` and so contributes no edge.
pub fn child_edge(d: &Keyed<String, NodeRec>) -> Option<Keyed<String, (u32, String)>> {
    let parent = d.val.parent_id.clone()?;
    Some(Keyed::new(parent, (d.val.child_index, d.val.id.clone())))
}

/// Feeds the `text` BM25 sink: node name plus (if TEXT) its `characters`,
/// keyed by node id. Nodes with no searchable text drop out.
pub fn text_doc(d: &Keyed<String, NodeRec>) -> Option<Keyed<String, String>> {
    let mut s = d.val.name.clone();
    if let Some(t) = &d.val.text {
        s.push(' ');
        s.push_str(t);
    }
    let s = s.trim().to_string();
    (!s.is_empty()).then(|| Keyed::new(d.val.id.clone(), s))
}

/// Feeds the `instances_of` inverted index: node id -> the component id it
/// instances (INSTANCE nodes only).
pub fn instance_edge(d: &Keyed<String, NodeRec>) -> Option<Keyed<String, String>> {
    d.val
        .component_id
        .clone()
        .map(|c| Keyed::new(d.val.id.clone(), c))
}

/// Feeds the `styled_by` inverted index: node id -> each style id it
/// references (fill, text, effect, grid).
pub fn style_edges(d: &Keyed<String, NodeRec>) -> Vec<Keyed<String, String>> {
    d.val
        .style_refs
        .iter()
        .map(|(_, style_id)| Keyed::new(d.val.id.clone(), style_id.clone()))
        .collect()
}

/// Feeds the `bound_to` inverted index: node id -> each variable id bound
/// somewhere on it (one edge per distinct variable, even if bound at
/// multiple property paths).
pub fn variable_edges(d: &Keyed<String, NodeRec>) -> Vec<Keyed<String, String>> {
    // No dedup here: `bound_variables` can bind the same variable at
    // multiple property paths, producing duplicate (node id, var id) edges.
    // We don't sort/dedup them because `InvertedIndex` is set-semantic —
    // duplicate edges collapse to the same membership and are harmless.
    d.val
        .bound_variables
        .iter()
        .map(|(_, var_id)| Keyed::new(d.val.id.clone(), var_id.clone()))
        .collect()
}

/// Feeds the `by_type` inverted index: node id -> its Figma node type.
pub fn type_edge(d: &Keyed<String, NodeRec>) -> Keyed<String, String> {
    Keyed::new(d.val.id.clone(), d.val.node_type.clone())
}

/// Defines a `fn(&Keyed<Id, Rec>) -> Option<Keyed<String, $rec>>` that keeps
/// only the matching `Id`/`Rec` variant pair, keyed by its own id — one such
/// branch per non-node table (`components`, `component_sets`, `styles`,
/// `variables`, `variable_collections`).
macro_rules! rec_branch {
    ($name:ident, $idvar:ident, $recvar:ident, $rec:ty) => {
        pub fn $name(d: &Keyed<Id, Rec>) -> Option<Keyed<String, $rec>> {
            match (&d.key, &d.val) {
                (Id::$idvar(k), Rec::$recvar(r)) => Some(Keyed::new(k.clone(), r.clone())),
                _ => None,
            }
        }
    };
}
rec_branch!(
    component_only,
    Component,
    Component,
    crate::model::ComponentRec
);
rec_branch!(
    component_set_only,
    ComponentSet,
    ComponentSet,
    crate::model::ComponentSetRec
);
rec_branch!(style_only, Style, Style, crate::model::StyleRec);
rec_branch!(variable_only, Variable, Variable, crate::model::VariableRec);
rec_branch!(
    collection_only,
    VariableCollection,
    VariableCollection,
    crate::model::VariableCollectionRec
);
rec_branch!(
    proxy_cache_only,
    ProxyCache,
    ProxyCache,
    crate::model::ProxyCacheRec
);

/// Feeds the `meta` table: the single [`FileMeta`] row, keyed by `0u8`
/// (not `()`: `()` postcard-encodes to zero bytes and the store forbids
/// empty keys).
pub fn meta_only(d: &Keyed<Id, Rec>) -> Option<Keyed<u8, FileMeta>> {
    match &d.val {
        Rec::Meta(m) => Some(Keyed::new(0u8, m.clone())),
        _ => None,
    }
}

/// Feeds the `mirror_config` table: the single [`MirrorConfigRec`] row
/// (v0.0.2 spec §4), keyed by `0u8` — same reasoning as [`meta_only`].
pub fn mirror_config_only(d: &Keyed<Id, Rec>) -> Option<Keyed<u8, MirrorConfigRec>> {
    match &d.val {
        Rec::MirrorConfig(m) => Some(Keyed::new(0u8, m.clone())),
        _ => None,
    }
}

/// The full figmog pipeline. Sink names are frozen on-disk schema.
#[macro_export]
macro_rules! figmog_pipeline {
    () => {{
        use fold::pipeline::{FilterMap, FlatMap, Map, terminal};
        (
            FilterMap::new(
                $crate::store::node_only,
                (
                    terminal::Table::new("nodes"),
                    FilterMap::new(
                        $crate::store::child_edge,
                        terminal::Multimap::new("children"),
                    ),
                    FilterMap::new($crate::store::text_doc, terminal::search::Bm25::new("text")),
                    FilterMap::new(
                        $crate::store::instance_edge,
                        terminal::InvertedIndex::new("instances_of"),
                    ),
                    FlatMap::new(
                        $crate::store::style_edges,
                        terminal::InvertedIndex::new("styled_by"),
                    ),
                    FlatMap::new(
                        $crate::store::variable_edges,
                        terminal::InvertedIndex::new("bound_to"),
                    ),
                    Map::new(
                        $crate::store::type_edge,
                        terminal::InvertedIndex::new("by_type"),
                    ),
                ),
            ),
            FilterMap::new(
                $crate::store::component_only,
                terminal::Table::new("components"),
            ),
            FilterMap::new(
                $crate::store::component_set_only,
                terminal::Table::new("component_sets"),
            ),
            FilterMap::new($crate::store::style_only, terminal::Table::new("styles")),
            FilterMap::new(
                $crate::store::variable_only,
                terminal::Table::new("variables"),
            ),
            FilterMap::new(
                $crate::store::collection_only,
                terminal::Table::new("variable_collections"),
            ),
            FilterMap::new($crate::store::meta_only, terminal::Table::new("meta")),
            FilterMap::new(
                $crate::store::proxy_cache_only,
                terminal::Table::new("proxy_cache"),
            ),
            FilterMap::new(
                $crate::store::mirror_config_only,
                terminal::Table::new("mirror_config"),
            ),
        )
    }};
}

/// Open (or create) the figmog store at `$path`.
#[macro_export]
macro_rules! open_store {
    ($path:expr) => {
        ::fold::stream::KeyedStream::<$crate::model::Id, $crate::model::Rec, _>::new(
            $path,
            $crate::figmog_pipeline!(),
        )
    };
}

// ---- sync ----

/// What one sync did, per record.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Churn {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// Apply a flattened file in one atomic transaction: upsert every record
/// and the meta row, then remove previously-stored ids that vanished. The
/// meta row is always exempt from the sweep; variables and collections are
/// exempt too *unless* the caller opted them in via `prior_sweepable`
/// (`collect_variable_ids`, spec §12 — an Enterprise `variables_local`
/// pull makes them file state for that cycle).
pub fn sync<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    prior_sweepable: &BTreeSet<Id>,
    flattened: &Flattened,
    synced_at_unix_ms: u64,
) -> Churn {
    let meta = FileMeta {
        name: flattened.file.name.clone(),
        version: flattened.file.version.clone(),
        last_modified: flattened.file.last_modified.clone(),
        synced_at_unix_ms,
    };
    let live: BTreeSet<&Id> = flattened.recs.iter().map(|(id, _)| id).collect();

    let mut churn = Churn::default();
    st.wtx(|tx| {
        for (id, rec) in &flattened.recs {
            match tx.upsert(id, rec) {
                None => churn.added += 1,
                Some(old) if old == *rec => churn.unchanged += 1,
                Some(_) => churn.changed += 1,
            }
        }
        tx.upsert(&Id::Meta, &Rec::Meta(meta));
        for id in prior_sweepable {
            if !live.contains(id) {
                // The meta row is never sweepable — `collect_sweepable` and
                // `collect_variable_ids` both draw only from the
                // nodes/components/component_sets/styles/variables/
                // collections tables, never `meta`.
                debug_assert!(!matches!(id, Id::Meta));
                if tx.remove(id).is_some() {
                    churn.removed += 1;
                }
            }
        }
    });
    churn
}

/// Gather the sweepable id set from the four table readers. Call inside
/// `rtx` *before* `sync` (single-writer process: no write races).
pub fn collect_sweepable<R: fold::stream::Readable>(
    nodes: &fold::pipeline::terminal::TableReader<'_, R, String, NodeRec>,
    components: &fold::pipeline::terminal::TableReader<'_, R, String, crate::model::ComponentRec>,
    component_sets: &fold::pipeline::terminal::TableReader<
        '_,
        R,
        String,
        crate::model::ComponentSetRec,
    >,
    styles: &fold::pipeline::terminal::TableReader<'_, R, String, crate::model::StyleRec>,
) -> BTreeSet<Id> {
    let mut out = BTreeSet::new();
    out.extend(nodes.iter().map(|(k, _)| Id::Node(k)));
    out.extend(components.iter().map(|(k, _)| Id::Component(k)));
    out.extend(component_sets.iter().map(|(k, _)| Id::ComponentSet(k)));
    out.extend(styles.iter().map(|(k, _)| Id::Style(k)));
    out
}

/// Gather the currently-*stored* variable + collection ids (spec §12:
/// Enterprise variables in `pull`). Unlike [`collect_sweepable`], callers
/// must union this into the prior/sweepable set only on a pull that fetched
/// an Enterprise variables export *this cycle* — the `variables_local` call
/// returned `Some(..)` and its records were flattened into the same
/// `flattened.recs` passed to `sync`. On the `Ok(None)` (non-Enterprise or
/// import-only) path, callers must not call this — stored variables then
/// stay outside `sync`'s live/sweep accounting entirely, exactly as v1.
pub fn collect_variable_ids<R: fold::stream::Readable>(
    variables: &fold::pipeline::terminal::TableReader<'_, R, String, crate::model::VariableRec>,
    collections: &fold::pipeline::terminal::TableReader<
        '_,
        R,
        String,
        crate::model::VariableCollectionRec,
    >,
) -> BTreeSet<Id> {
    let mut out = BTreeSet::new();
    out.extend(variables.iter().map(|(k, _)| Id::Variable(k)));
    out.extend(collections.iter().map(|(k, _)| Id::VariableCollection(k)));
    out
}

// ---- proxy cache eviction (spec §12) ----
//
// Cache eviction is deliberately NOT folded into `sync`'s sweep: the sweep
// removes ids that vanished from the *newly flattened file*, whereas cache
// rows go stale only because the file *version* moved, independent of
// which nodes/components/styles are still live. Keeping it a separate
// pass means `sync`'s churn accounting (and every test that pins its
// numbers) is untouched by this feature. Callers run a version-changing
// pull, then `stale_cache_ids` + `evict_stale_cache` in a follow-up step.

/// `ProxyCache` rows whose `file_version` no longer matches
/// `current_version` — the sweep set for [`evict_stale_cache`].
pub fn stale_cache_ids<R: fold::stream::Readable>(
    cache: &fold::pipeline::terminal::TableReader<'_, R, String, ProxyCacheRec>,
    current_version: &str,
) -> Vec<Id> {
    cache
        .iter()
        .filter(|(_, rec)| rec.file_version != current_version)
        .map(|(k, _)| Id::ProxyCache(k))
        .collect()
}

/// Remove `stale` cache rows in one write transaction. Never touches
/// variables, collections, or the meta row — pass only ids gathered by
/// [`stale_cache_ids`].
pub fn evict_stale_cache<P: Push<Keyed<Id, Rec>>>(st: &mut KeyedStream<Id, Rec, P>, stale: &[Id]) {
    st.wtx(|tx| {
        for id in stale {
            debug_assert!(matches!(id, Id::ProxyCache(_)));
            tx.remove(id);
        }
    });
}

// ---- sticky vector geometry (v0.0.2 spec §4) ----
//
// `mirror_config` is exempt from `sync`'s sweep the same way `meta` is:
// `collect_sweepable` (above) draws only from the nodes/components/
// component_sets/styles table readers, never `mirror_config` — this is a
// structural property of that function's own argument list, not a runtime
// check, so there's nothing to `debug_assert` against inside `sync` for it
// (unlike `evict_stale_cache`'s `ProxyCache`-only assertion, which guards a
// *caller-supplied* id list against passing the wrong table).

/// Read the stored geometry flag ([`MirrorConfigRec::geometry`]), or
/// `false` when no row has ever been upserted (every pre-v0.0.2 store, and
/// any store never pulled with `--geometry`).
pub fn read_geometry<R: fold::stream::Readable>(
    cfg: &fold::pipeline::terminal::TableReader<'_, R, u8, MirrorConfigRec>,
) -> bool {
    cfg.get(&0).map(|c| c.geometry).unwrap_or(false)
}

/// Upsert the sticky geometry flag in its own write transaction — a
/// separate `wtx` from `sync`'s (mirroring `evict_stale_cache`'s own
/// separation, see that section's note above): this is config bookkeeping
/// about the *pull itself*, not part of the flattened file's record set
/// `sync` reconciles.
pub fn upsert_mirror_config<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    geometry: bool,
) {
    st.wtx(|tx| {
        tx.upsert(
            &Id::MirrorConfig,
            &Rec::MirrorConfig(MirrorConfigRec { geometry }),
        );
    });
}

/// What a pull should request from Figma, given this call's own override
/// (`--geometry` / `figmog_open`'s `geometry` arg / a plain re-pull's
/// implicit `false`) and whatever's already stored: once either side is
/// `true`, geometry stays on (spec §4 — sticky; the documented way back
/// off is `pull --fresh`, whose callers pass `stored = false` here without
/// even reading the about-to-be-wiped store).
pub fn effective_geometry(flag: bool, stored: bool) -> bool {
    flag || stored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_geometry_is_sticky_once_either_side_is_true() {
        assert!(!effective_geometry(false, false));
        assert!(effective_geometry(true, false));
        assert!(effective_geometry(false, true));
        assert!(effective_geometry(true, true));
    }

    /// Store-level proof that the stored flag alone drives a later pull's
    /// choice (spec §4 stickiness): absent config reads `false`, an upsert
    /// makes it stick, and reading it again — the exact shape
    /// `cli::pull::do_pull`/`sessions::open_session_at`'s pull closure both
    /// perform before deciding what to request — reflects the write.
    #[test]
    fn mirror_config_defaults_false_and_persists_once_upserted() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(&dir.path().join("db"));

        let absent = st.rtx(|(.., mirror_config)| read_geometry(&mirror_config));
        assert!(!absent, "no row upserted yet ⇒ geometry defaults false");

        upsert_mirror_config(&mut st, true);
        let now_true = st.rtx(|(.., mirror_config)| read_geometry(&mirror_config));
        assert!(now_true, "the upserted flag must be what a later read sees");

        // Sweep-exempt: `mirror_config` isn't among `collect_sweepable`'s
        // sources, so an ordinary `sync` sweep (even one whose live set is
        // empty) must never remove it — mirroring `meta`'s own exemption.
        let flattened = Flattened {
            recs: Vec::new(),
            file: crate::flatten::FileInfo {
                name: "F".into(),
                version: "1".into(),
                last_modified: "t".into(),
            },
        };
        let prior = st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
            collect_sweepable(&nodes, &components, &component_sets, &styles)
        });
        sync(&mut st, &prior, &flattened, 0);
        let still_true = st.rtx(|(.., mirror_config)| read_geometry(&mirror_config));
        assert!(still_true, "mirror_config must survive an unrelated sweep");
    }
}
