#![recursion_limit = "256"]

mod common;

use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

use figmog::flatten::flatten_file;
use figmog::model::{Id, Rec};
use figmog::store::{Churn, sync};
use fold::pipeline::{Keyed, Map};

/// Open a store whose pipeline is fronted by a delta probe: every push
/// into the graph bumps the counter. Zero churn must mean zero pushes.
macro_rules! open_probed {
    ($path:expr, $counter:expr) => {{
        let c = $counter.clone();
        ::fold::stream::KeyedStream::<Id, Rec, _>::new(
            $path,
            Map::new(
                move |d: &Keyed<Id, Rec>| {
                    c.set(c.get() + 1);
                    d.clone()
                },
                figmog::figmog_pipeline!(),
            ),
        )
    }};
}

fn pull(
    st: &mut fold::stream::KeyedStream<Id, Rec, impl fold::pipeline::Push<Keyed<Id, Rec>>>,
    fixture: &serde_json::Value,
) -> Churn {
    // NOTE: `impl Trait` in argument position works here because we only
    // use write-path (upsert/remove) APIs; readers stay at the call site.
    let flattened = flatten_file(fixture).unwrap();
    let prior = BTreeSet::new(); // overridden by tests that need the sweep
    sync(st, &prior, &flattened, 1_000)
}

#[test]
fn initial_pull_populates_every_sink() {
    let dir = tempfile::tempdir().unwrap();
    let counter = Rc::new(Cell::new(0usize));
    let mut st = open_probed!(dir.path().join("db"), counter);

    let churn = pull(&mut st, &common::fixture_v1());
    assert_eq!(
        churn,
        Churn {
            added: 18,
            changed: 0,
            removed: 0,
            unchanged: 0
        }
    );
    // 18 records + 1 meta row, all fresh inserts -> 19 pushes
    assert_eq!(counter.get(), 19);

    st.rtx(
        |(
            (nodes, children, text, instances_of, styled_by, bound_to, by_type),
            components,
            component_sets,
            styles,
            _vars,
            _colls,
            meta,
            _cache,
            _mirror_config,
            _images,
        )| {
            assert_eq!(nodes.iter().count(), 12);
            assert_eq!(nodes.get(&"1:2".to_string()).unwrap().name, "Title");

            let mut kids = children.get(&"1:1".to_string());
            kids.sort();
            assert_eq!(kids, vec![(0, "1:2".to_string()), (1, "1:3".to_string())]);

            let hits = text.search("garden", 5);
            assert!(
                hits.iter().any(|h| h.val == "1:2"),
                "bm25 finds the title text"
            );

            assert_eq!(
                instances_of.search(&"2:2".to_string()),
                vec!["1:3".to_string()]
            );
            assert_eq!(
                styled_by.search(&"S:2".to_string()),
                vec!["1:2".to_string()]
            );
            assert_eq!(
                bound_to.search(&"VariableID:100".to_string()),
                vec!["1:1".to_string()]
            );

            let mut texts = by_type.search(&"TEXT".to_string());
            texts.sort();
            assert_eq!(texts, vec!["1:2".to_string()]);

            assert_eq!(components.iter().count(), 3);
            assert_eq!(component_sets.iter().count(), 1);
            assert_eq!(styles.iter().count(), 2);
            let m = meta.get(&0).unwrap();
            assert_eq!(m.version, "100");
            assert_eq!(m.synced_at_unix_ms, 1_000);
        },
    );
}

#[test]
fn identical_repull_causes_zero_churn() {
    let dir = tempfile::tempdir().unwrap();
    let counter = Rc::new(Cell::new(0usize));
    let mut st = open_probed!(dir.path().join("db"), counter);

    pull(&mut st, &common::fixture_v1());
    counter.set(0);
    let churn = pull(&mut st, &common::fixture_v1()); // same synced_at too
    assert_eq!(
        churn,
        Churn {
            added: 0,
            changed: 0,
            removed: 0,
            unchanged: 18
        }
    );
    assert_eq!(
        counter.get(),
        0,
        "no delta may enter the graph on an identical re-pull"
    );
}

#[test]
fn reopen_resumes_persisted_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    {
        let mut st = figmog::open_store!(&db);
        let flattened = flatten_file(&common::fixture_v1()).unwrap();
        sync(&mut st, &BTreeSet::new(), &flattened, 1_000);
    }
    let st = figmog::open_store!(&db);
    st.rtx(|((nodes, ..), _, _, _, _, _, _, _, _, _)| {
        assert_eq!(nodes.iter().count(), 12);
    });
}

/// Pull v2 over v1 with the sweep enabled, capturing probe deltas.
fn pull_with_sweep(
    st: &mut fold::stream::KeyedStream<Id, Rec, impl fold::pipeline::Push<Keyed<Id, Rec>>>,
    fixture: &serde_json::Value,
    prior: BTreeSet<Id>,
    synced_at: u64,
) -> Churn {
    let flattened = flatten_file(fixture).unwrap();
    sync(st, &prior, &flattened, synced_at)
}

#[test]
fn v1_to_v2_minimal_churn_and_index_consistency() {
    let dir = tempfile::tempdir().unwrap();
    let counter = Rc::new(Cell::new(0usize));
    let mut st = open_probed!(dir.path().join("db"), counter);
    pull(&mut st, &common::fixture_v1());

    let prior = st.rtx(
        |((nodes, ..), components, component_sets, styles, _, _, _, _, _, _)| {
            figmog::store::collect_sweepable(&nodes, &components, &component_sets, &styles)
        },
    );
    counter.set(0);
    let churn = pull_with_sweep(&mut st, &common::fixture_v2(), prior, 1_000);

    // v2 has 18 records: 12 nodes (12 - 1:9 + 1:4) + 3 components + 1 set
    // + 2 styles. changed: 1:2 (rename), 1:3 (variant repoint). added: 1:4.
    // removed: 1:9. unchanged: 18 - 1 - 2 = 15 (meta row is not counted).
    assert_eq!(
        churn,
        Churn {
            added: 1,
            changed: 2,
            removed: 1,
            unchanged: 15
        }
    );
    // pushes: changed 2×2 + added 1 + removed 1 + meta retract/insert 2 = 8
    assert_eq!(counter.get(), 8);

    st.rtx(
        |(
            (nodes, children, text, instances_of, _styled, _bound, by_type),
            _c,
            _cs,
            _s,
            _v,
            _vc,
            meta,
            _cache,
            _mirror_config,
            _images,
        )| {
            // rename re-indexed in bm25
            assert!(text.search("Headline", 5).iter().any(|h| h.val == "1:2"));
            assert!(!text.search("Title", 5).iter().any(|h| h.val == "1:2"));
            // deleted node gone everywhere
            assert!(nodes.get(&"1:9".to_string()).is_none());
            assert!(
                !by_type
                    .search(&"RECTANGLE".to_string())
                    .contains(&"1:9".to_string())
            );
            let kids = children.get(&"0:1".to_string());
            assert!(!kids.iter().any(|(_, id)| id == "1:9"));
            // instance repoint moved the inverted index posting
            assert_eq!(
                instances_of.search(&"2:2".to_string()),
                Vec::<String>::new()
            );
            assert_eq!(
                instances_of.search(&"2:3".to_string()),
                vec!["1:3".to_string()]
            );
            // new node present
            assert_eq!(nodes.get(&"1:4".to_string()).unwrap().name, "Subtitle");
            assert!(text.search("Planting", 5).iter().any(|h| h.val == "1:4"));
            assert_eq!(meta.get(&0).unwrap().version, "101");
        },
    );
}

#[test]
fn sweep_never_touches_variables() {
    use figmog::model::{VariableCollectionRec, VariableRec};
    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));
    pull(&mut st, &common::fixture_v1());
    // hand-insert an imported variable, then re-pull with a full sweep set
    st.wtx(|tx| {
        tx.upsert(
            &Id::Variable("VariableID:100".into()),
            &Rec::Variable(VariableRec {
                id: "VariableID:100".into(),
                name: "color/bg".into(),
                resolved_type: "COLOR".into(),
                collection_id: "VC:1".into(),
                values_by_mode: vec![("M:1".into(), "{\"r\":0.06}".into())],
                description: String::new(),
                scopes: vec![],
            }),
        );
        tx.upsert(
            &Id::VariableCollection("VC:1".into()),
            &Rec::VariableCollection(VariableCollectionRec {
                id: "VC:1".into(),
                name: "core".into(),
                modes: vec![("M:1".into(), "light".into())],
                default_mode_id: "M:1".into(),
            }),
        );
    });
    let prior = st.rtx(
        |((nodes, ..), components, component_sets, styles, _, _, _, _, _, _)| {
            figmog::store::collect_sweepable(&nodes, &components, &component_sets, &styles)
        },
    );
    pull_with_sweep(&mut st, &common::fixture_v2(), prior, 2_000);
    st.rtx(|(_, _, _, _, vars, colls, _, _, _, _)| {
        assert!(vars.get(&"VariableID:100".to_string()).is_some());
        assert!(colls.get(&"VC:1".to_string()).is_some());
    });
}

#[test]
fn panicking_transaction_rolls_back_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));
    pull(&mut st, &common::fixture_v1());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Node("9:9".into()),
                &Rec::Node(figmog::model::NodeRec {
                    id: "9:9".into(),
                    parent_id: Some("0:1".into()),
                    child_index: 7,
                    page_id: "0:1".into(),
                    node_type: "FRAME".into(),
                    name: "doomed".into(),
                    visible: true,
                    text: None,
                    component_id: None,
                    component_properties: vec![],
                    property_definitions: None,
                    style_refs: vec![],
                    bound_variables: vec![],
                    abs_bounds: None,
                    raw: "{}".into(),
                }),
            );
            panic!("mid-transaction failure");
        })
    }));
    assert!(result.is_err());
    st.rtx(|((nodes, ..), _, _, _, _, _, meta, _, _, _)| {
        assert!(
            nodes.get(&"9:9".to_string()).is_none(),
            "aborted upsert must not persist"
        );
        assert_eq!(nodes.iter().count(), 12);
        assert_eq!(meta.get(&0).unwrap().version, "100");
    });
}

/// Proxy cache rows (spec §12): survive a same-version repull, are evicted
/// by `stale_cache_ids` + `evict_stale_cache` after a version-changing
/// pull, and never perturb (or are perturbed by) the variable sweep-exempt
/// set — a manually imported variable survives both steps.
#[test]
fn proxy_cache_survives_same_version_and_is_evicted_on_version_change() {
    use figmog::model::VariableRec;

    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));
    pull(&mut st, &common::fixture_v1());

    // Hand-insert an imported variable, same as `sweep_never_touches_variables`.
    st.wtx(|tx| {
        tx.upsert(
            &Id::Variable("VariableID:100".into()),
            &Rec::Variable(VariableRec {
                id: "VariableID:100".into(),
                name: "color/bg".into(),
                resolved_type: "COLOR".into(),
                collection_id: "VC:1".into(),
                values_by_mode: vec![("M:1".into(), "{\"r\":0.06}".into())],
                description: String::new(),
                scopes: vec![],
            }),
        );
    });

    let tool = "get_code";
    let args = "{\"nodeId\":\"1:2\"}";
    let content = serde_json::json!({"content": [{"type": "text", "text": "<div/>"}]});
    figmog::cache::store(&mut st, tool, args, "100", &content).unwrap();

    // Cache row present right after the write.
    st.rtx(|(_, _, _, _, _, _, _, cache, _, _)| {
        assert_eq!(
            figmog::cache::lookup(&cache, tool, args, "100"),
            Some(content.clone())
        );
    });

    // Identical re-pull: file version unchanged -> cache row must survive,
    // and so must the imported variable.
    pull(&mut st, &common::fixture_v1());
    st.rtx(|(_, _, _, _, vars, _, _, cache, _, _)| {
        assert_eq!(
            figmog::cache::lookup(&cache, tool, args, "100"),
            Some(content.clone()),
            "cache row must survive a same-version repull"
        );
        assert!(vars.get(&"VariableID:100".to_string()).is_some());
    });

    // v1 -> v2 pull (version "100" -> "101") with the sweep enabled.
    let prior = st.rtx(
        |((nodes, ..), components, component_sets, styles, _, _, _, _, _, _)| {
            figmog::store::collect_sweepable(&nodes, &components, &component_sets, &styles)
        },
    );
    pull_with_sweep(&mut st, &common::fixture_v2(), prior, 2_000);

    // The cache row is now stale (its file_version is still "100").
    let stale =
        st.rtx(|(_, _, _, _, _, _, _, cache, _, _)| figmog::store::stale_cache_ids(&cache, "101"));
    assert_eq!(
        stale,
        vec![Id::ProxyCache(figmog::cache::cache_key(tool, args))]
    );
    figmog::store::evict_stale_cache(&mut st, &stale);

    st.rtx(|(_, _, _, _, vars, _, _, cache, _, _)| {
        assert!(
            figmog::cache::lookup(&cache, tool, args, "101").is_none(),
            "stale cache row must be gone after eviction"
        );
        assert!(
            cache.get(&figmog::cache::cache_key(tool, args)).is_none(),
            "eviction removes the row outright, not just the version-gated read"
        );
        assert!(
            vars.get(&"VariableID:100".to_string()).is_some(),
            "cache eviction must never touch sweep-exempt variables"
        );
    });
}

/// `images` rows (v0.0.2 spec §5) join the exact same version-triggered
/// eviction path as `proxy_cache` (`stale_image_ids` / `evict_stale_cache`
/// — see the test right above this one for the `proxy_cache` half): a
/// row survives a same-version repull and is evicted, record-level, once
/// the file version moves on.
#[test]
fn image_blob_survives_same_version_and_is_evicted_on_version_change() {
    use figmog::images::image_key;
    use figmog::model::ImageBlobRec;

    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));
    pull(&mut st, &common::fixture_v1());

    let key = image_key("render", "1:2", "png", 1000);
    let rec = ImageBlobRec {
        key_hash: key.clone(),
        kind: "render".into(),
        subject: "1:2".into(),
        format: "png".into(),
        scale_milli: 1000,
        file_version: "100".into(),
        bytes: vec![1, 2, 3, 4],
    };
    st.wtx(|tx| {
        tx.upsert(&Id::ImageBlob(key.clone()), &Rec::ImageBlob(rec.clone()));
    });

    // Row present right after the write.
    st.rtx(|(_, _, _, _, _, _, _, _, _, images)| {
        assert_eq!(images.get(&key), Some(rec.clone()));
    });

    // Identical re-pull: file version unchanged -> row must survive.
    pull(&mut st, &common::fixture_v1());
    st.rtx(|(_, _, _, _, _, _, _, _, _, images)| {
        assert_eq!(
            images.get(&key),
            Some(rec.clone()),
            "image row must survive a same-version repull"
        );
    });

    // v1 -> v2 pull (version "100" -> "101") with the sweep enabled.
    let prior = st.rtx(
        |((nodes, ..), components, component_sets, styles, _, _, _, _, _, _)| {
            figmog::store::collect_sweepable(&nodes, &components, &component_sets, &styles)
        },
    );
    pull_with_sweep(&mut st, &common::fixture_v2(), prior, 2_000);

    // The row is now stale (its file_version is still "100").
    let stale = st
        .rtx(|(_, _, _, _, _, _, _, _, _, images)| figmog::store::stale_image_ids(&images, "101"));
    assert_eq!(stale, vec![Id::ImageBlob(key.clone())]);
    figmog::store::evict_stale_cache(&mut st, &stale);

    st.rtx(|(_, _, _, _, _, _, _, _, _, images)| {
        assert!(
            images.get(&key).is_none(),
            "stale image row must be gone after eviction"
        );
    });
}
