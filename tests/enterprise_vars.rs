//! Sync-level tests for opportunistic Enterprise variables (spec §12):
//! `pull` additionally calling `GET /v1/files/:key/variables/local` folds
//! the export's records into the same sync **and makes them sweepable for
//! that pull**, while an `Ok(None)` response (non-Enterprise plan, or a
//! `--from-file` pull that never calls the network endpoint at all) leaves
//! v1 behavior — import/inference, sweep-exempt — untouched.
//!
//! These tests prove the record-level wiring contract
//! (`parse_variables_export` into `flattened.recs`,
//! `store::collect_variable_ids` into the prior/sweepable set) directly,
//! since a fake `FigmaApi` is awkward to thread through at the sync layer.
//! The `Ok(None)`/403-404 gating on the network call itself is covered by
//! `api::tests` instead.

#![recursion_limit = "256"]

mod common;

use std::collections::BTreeSet;

use figmog::model::{Id, Rec};
use figmog::store::{Churn, collect_sweepable, collect_variable_ids, sync};
use figmog::vars::parse_variables_export;

fn export() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/variables-export.json")).unwrap()
}

#[test]
fn enterprise_export_syncs_with_zero_churn_on_identical_repull() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));

    let var_recs = parse_variables_export(&export()).unwrap();

    // First pull: nodes + the Enterprise export together (the
    // `variables_local` `Some(v)` path).
    let mut flattened = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    flattened.recs.extend(var_recs.clone());
    let prior1 = st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
        collect_sweepable(&nodes, &components, &component_sets, &styles)
        // No stored variables yet on the very first pull, so
        // `collect_variable_ids` would be empty here regardless — this
        // matches what `do_pull` does (it still unions it in, this test
        // just documents that the initial set is empty).
    });
    let churn1 = sync(&mut st, &prior1, &flattened, 1_000);
    assert_eq!(
        churn1,
        Churn {
            added: 18 + 5, // 18 node/component/set/style recs + 2 collections + 3 variables
            changed: 0,
            removed: 0,
            unchanged: 0
        }
    );

    // Second, identical pull: prior now unions in the stored variable ids
    // too (this pull's `variables_local` returned `Some` again).
    let mut flattened2 = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    flattened2.recs.extend(var_recs);
    let prior2 = st.rtx(
        |(
            (nodes, ..),
            components,
            component_sets,
            styles,
            variables,
            variable_collections,
            _,
            _,
            _,
            _,
        )| {
            let mut p = collect_sweepable(&nodes, &components, &component_sets, &styles);
            p.extend(collect_variable_ids(&variables, &variable_collections));
            p
        },
    );
    let churn2 = sync(&mut st, &prior2, &flattened2, 2_000);
    assert_eq!(
        churn2,
        Churn {
            added: 0,
            changed: 0,
            removed: 0,
            unchanged: 23
        },
        "identical Enterprise export re-pull must cause zero churn, variables included"
    );
}

#[test]
fn variable_removed_upstream_is_swept_when_export_present() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));

    let mut flattened = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    flattened
        .recs
        .extend(parse_variables_export(&export()).unwrap());
    let prior = st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
        collect_sweepable(&nodes, &components, &component_sets, &styles)
    });
    sync(&mut st, &prior, &flattened, 1_000);
    st.rtx(|(_, _, _, _, variables, _, _, _, _, _)| {
        assert_eq!(variables.iter().count(), 3);
    });

    // Second pull's export drops "VariableID:200" (e.g. deleted upstream).
    // Its collection ("VariableCollectionId:2") is left in place so only
    // the variable itself is expected to be swept.
    let mut export2 = export();
    export2["meta"]["variables"]
        .as_object_mut()
        .unwrap()
        .remove("VariableID:200");
    let mut flattened2 = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    flattened2
        .recs
        .extend(parse_variables_export(&export2).unwrap());

    let prior2 = st.rtx(
        |(
            (nodes, ..),
            components,
            component_sets,
            styles,
            variables,
            variable_collections,
            _,
            _,
            _,
            _,
        )| {
            let mut p = collect_sweepable(&nodes, &components, &component_sets, &styles);
            p.extend(collect_variable_ids(&variables, &variable_collections));
            p
        },
    );
    let churn = sync(&mut st, &prior2, &flattened2, 2_000);
    assert_eq!(churn.removed, 1, "the dropped variable must be swept");

    st.rtx(|(_, _, _, _, variables, collections, _, _, _, _)| {
        assert!(
            variables.get(&"VariableID:200".to_string()).is_none(),
            "removed-upstream variable must be gone"
        );
        assert!(
            variables.get(&"VariableID:100".to_string()).is_some(),
            "still-present variables must survive"
        );
        assert!(
            collections
                .get(&"VariableCollectionId:2".to_string())
                .is_some(),
            "the collection is still in the export, so it isn't swept"
        );
    });
}

#[test]
fn imported_variables_survive_pulls_with_no_export() {
    use figmog::model::{VariableCollectionRec, VariableRec};

    let dir = tempfile::tempdir().unwrap();
    let mut st = figmog::open_store!(dir.path().join("db"));

    let flattened = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    sync(&mut st, &BTreeSet::new(), &flattened, 1_000);

    // A variable landed in the store some other way (manual `import-variables`,
    // or an earlier Enterprise-synced pull) — same shape as
    // `sync.rs::sweep_never_touches_variables`.
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

    // Next pull is the `variables_local` `Ok(None)` path: no export recs
    // folded into `flattened`, and — critically — the prior set is built
    // WITHOUT `collect_variable_ids` (exactly what `do_pull` does when
    // `vars_resp` is `None`).
    let flattened2 = figmog::flatten::flatten_file(&common::fixture_v1()).unwrap();
    let prior = st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
        collect_sweepable(&nodes, &components, &component_sets, &styles)
    });
    let churn = sync(&mut st, &prior, &flattened2, 2_000);
    assert_eq!(churn.removed, 0, "no export means nothing to sweep");

    st.rtx(|(_, _, _, _, variables, collections, _, _, _, _)| {
        assert!(
            variables.get(&"VariableID:100".to_string()).is_some(),
            "v1 behavior intact: imported variables survive a pull with no Enterprise export"
        );
        assert!(collections.get(&"VC:1".to_string()).is_some());
    });
}
