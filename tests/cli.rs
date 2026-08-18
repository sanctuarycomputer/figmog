#![recursion_limit = "256"]

mod common;

use assert_cmd::Command;

/// Materialize fixture_v1 into a DB via `pull --from-file` and return the
/// (tempdir, db-arg) pair every read command needs.
fn fixture_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db = dir.path().join("db").display().to_string();
    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            &db,
        ])
        .assert()
        .success();
    (dir, db)
}

#[test]
fn pull_from_file_reports_churn_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db = dir.path().join("db").display().to_string();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            &db,
        ])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["added"], 18);

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            &db,
        ])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["unchanged"], 18);
    assert_eq!(v["added"], 0);
}

// ---- sticky vector geometry (v0.0.2 spec §4) ----

/// `pull --geometry` with a fixture that carries `fillGeometry` (what a
/// real `?geometry=paths` fetch would return on a vector node) keeps that
/// data in the node's raw JSON — `flatten`'s `raw` field is unknown-field-
/// preserving, so this is really proving the CLI plumbing (the flag
/// doesn't get stripped or cause an error) rather than the network
/// request itself, which `--from-file` never issues (see
/// `figmog::api::file_url`'s own unit tests for that half).
#[test]
fn pull_geometry_flag_keeps_fill_geometry_in_raw() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_with_geometry()).unwrap(),
    )
    .unwrap();
    let db = dir.path().join("db").display().to_string();

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--geometry",
            "--db",
            &db,
        ])
        .assert()
        .success();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "1:1", "--db", &db])
        .assert()
        .success();
    let node: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(
        node["fillGeometry"].is_array(),
        "fillGeometry must survive into raw: {node}"
    );
}

/// The stored half of stickiness (spec §4): a `--geometry` pull persists
/// the flag, and a later *plain* re-pull (no `--geometry`) must not clear
/// it. The actual network request choice this drives is proven separately
/// and purely (`store::tests::effective_geometry_is_sticky_once_either_
/// side_is_true`, `api::tests::file_url_adds_geometry_paths_only_when_
/// requested`) — `--from-file` never issues a request to inspect, so this
/// test proves the persisted config survives an ordinary re-pull
/// end-to-end instead. Reads the stored config directly off the store
/// (there's no CLI-surfaced way to read `mirror_config`), dropping each
/// handle before the next `figmog` subprocess reopens the same store —
/// fjall allows only one open per store per process at a time.
#[test]
fn pull_geometry_flag_is_sticky_across_a_later_plain_pull() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db_path = dir.path().join("db");
    let db = db_path.display().to_string();

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--geometry",
            "--db",
            &db,
        ])
        .assert()
        .success();
    {
        let st = figmog::open_store!(&db_path);
        let stored = st.rtx(|(.., mirror_config)| figmog::store::read_geometry(&mirror_config));
        assert!(stored, "the --geometry pull must persist the flag");
    }

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            &db,
        ])
        .assert()
        .success();
    {
        let st = figmog::open_store!(&db_path);
        let still_stored =
            st.rtx(|(.., mirror_config)| figmog::store::read_geometry(&mirror_config));
        assert!(still_stored, "a plain re-pull must not turn geometry off");
    }
}

/// The documented way back off (spec §4): `--fresh` wipes the store, so a
/// subsequent plain pull's config starts over at `false`.
#[test]
fn pull_fresh_turns_geometry_back_off() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db_path = dir.path().join("db");
    let db = db_path.display().to_string();

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--geometry",
            "--db",
            &db,
        ])
        .assert()
        .success();

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--fresh",
            "--db",
            &db,
        ])
        .assert()
        .success();

    let st = figmog::open_store!(&db_path);
    let stored = st.rtx(|(.., mirror_config)| figmog::store::read_geometry(&mirror_config));
    assert!(!stored, "--fresh must reset the sticky geometry flag");
}

#[test]
fn status_pages_tree_get_find() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    let status = run(&["status"]);
    assert_eq!(status["name"], "Fixture");
    assert_eq!(status["version"], "100");
    assert_eq!(status["nodes"], 12);

    let pages = run(&["pages"]);
    assert_eq!(pages.as_array().unwrap().len(), 3);
    assert_eq!(pages[0]["id"], "0:1");
    assert_eq!(pages[0]["name"], "Page 1");

    let tree = run(&["tree", "1:1"]);
    let kids = tree["children"].as_array().unwrap();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0]["id"], "1:2"); // numeric child order

    // node-id normalization: URL form accepted
    let get = run(&["get", "1-2"]);
    assert_eq!(get["name"], "Title");
    assert_eq!(get["characters"], "Welcome to the garden");

    let texts = run(&["find", "--type", "TEXT"]);
    assert_eq!(texts.as_array().unwrap().len(), 1);
    assert_eq!(texts[0]["id"], "1:2");

    // `--type` is case-insensitive (Figma types are stored uppercase).
    let texts_lower = run(&["find", "--type", "text"]);
    assert_eq!(texts_lower, texts);

    let on_page = run(&["find", "--type", "COMPONENT", "--page", "0:2"]);
    assert_eq!(on_page.as_array().unwrap().len(), 3); // 2:2, 2:3, 3:1
}

#[test]
fn get_unknown_node_fails_cleanly() {
    let (_dir, db) = fixture_db();
    Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "99:99", "--db", &db])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn get_unknown_node_error_is_json_on_stderr() {
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "99:99", "--db", &db])
        .assert()
        .failure()
        .code(1);
    let stderr = out.get_output().stderr.clone();
    let v: serde_json::Value = serde_json::from_slice(&stderr).unwrap_or_else(|e| {
        panic!(
            "stderr not JSON: {e}\nstderr: {}",
            String::from_utf8_lossy(&stderr)
        )
    });
    assert!(v["error"].as_str().unwrap().contains("99:99"));
}

#[test]
fn search_instances_components_styles_uses_vars() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    let hits = run(&["search", "garden"]);
    assert_eq!(hits[0]["id"], "1:2");
    assert!(hits[0]["score"].as_f64().unwrap() > 0.0);

    // by node id, by key, by set name (=> all variants' instances)
    for target in ["2:2", "key22", "Button"] {
        let inst = run(&["instances", target]);
        assert_eq!(inst[0]["id"], "1:3", "target={target}");
    }

    let comps = run(&["components"]);
    let sets = comps["sets"].as_array().unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0]["name"], "Button");
    assert_eq!(sets[0]["variants"].as_array().unwrap().len(), 2);
    let axes = &sets[0]["property_definitions"];
    assert_eq!(
        axes["Size"]["variantOptions"],
        serde_json::json!(["Large", "Small"])
    );
    assert_eq!(comps["components"].as_array().unwrap().len(), 1); // standalone only
    assert_eq!(comps["components"][0]["name"], "IconStar");

    let styles = run(&["styles"]);
    assert_eq!(styles.as_array().unwrap().len(), 2);
    assert_eq!(styles[0]["style_id"], "S:1");
    assert_eq!(styles[0]["uses"], 1);

    let styles = run(&["styles", "--values"]);
    assert_eq!(styles[1]["value"]["fontSize"], 32.0); // S:2 from consumer 1:2

    let uses = run(&["uses", "S:1"]);
    assert_eq!(uses[0]["id"], "1:1");
    let uses = run(&["uses", "VariableID:100"]);
    assert_eq!(uses[0]["id"], "1:1");

    let vars = run(&["vars"]);
    let arr = vars.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["variable_id"], "VariableID:100");
    assert_eq!(arr[0]["source"], "inferred");
}

#[test]
fn stats_path_text_where_at() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    let stats = run(&["stats"]);
    assert_eq!(stats["by_type"]["TEXT"], 1);
    assert_eq!(stats["by_page"]["0:1"], 4); // 1:1, 1:2, 1:3, 1:9
    assert_eq!(stats["totals"]["components"], 3);
    assert_eq!(stats["totals"]["component_sets"], 1);
    assert_eq!(stats["totals"]["styles"], 2);
    assert_eq!(stats["max_depth"], 3); // document -> canvas -> frame -> text

    let path = run(&["path", "1-2"]);
    let ids: Vec<&str> = path
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["0:0", "0:1", "1:1", "1:2"]);

    let text = run(&["text"]);
    let rows = text.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "1:2");
    assert_eq!(rows[0]["characters"], "Welcome to the garden");

    let where_layout = run(&["where", "--pointer", "/layoutMode", "--equals", "VERTICAL"]);
    let ids: Vec<&str> = where_layout
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["1:1"]);

    let where_font = run(&["where", "--pointer", "/style/fontSize", "--equals", "32.0"]);
    let ids: Vec<&str> = where_font
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["1:2"]);

    let at = run(&["at", "--x", "10", "--y", "10"]);
    let ids: Vec<&str> = at
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"1:1"), "ids={ids:?}");
    // nodes without abs_bounds (e.g. the DOCUMENT/CANVAS/TEXT nodes here)
    // never appear.
    assert!(!ids.contains(&"1:2"), "ids={ids:?}");
}

/// A corrupted store with a `parent_id` cycle must not hang `path` or
/// `stats` — both walk `parent_id` chains, and both become MCP tool bodies
/// inside `figmog serve`'s single-threaded loop, where a hang would stall
/// the whole server. Hand-upsert two nodes whose parents point at each
/// other (same hand-upsert pattern as `tests/sync.rs`), bypassing
/// `flatten`/`sync` entirely so the cycle can't be prevented upstream.
#[test]
fn parent_cycle_does_not_hang_path_or_stats() {
    use figmog::model::{Id, NodeRec, Rec};

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let mut st = figmog::open_store!(&db);
    let node = |id: &str, parent: &str| NodeRec {
        id: id.into(),
        parent_id: Some(parent.into()),
        child_index: 0,
        page_id: "0:1".into(),
        node_type: "FRAME".into(),
        name: id.into(),
        visible: true,
        text: None,
        component_id: None,
        component_properties: vec![],
        property_definitions: None,
        style_refs: vec![],
        bound_variables: vec![],
        abs_bounds: None,
        raw: "{}".into(),
    };
    st.wtx(|tx| {
        tx.upsert(&Id::Node("A:1".into()), &Rec::Node(node("A:1", "B:1")));
        tx.upsert(&Id::Node("B:1".into()), &Rec::Node(node("B:1", "A:1")));
    });
    drop(st); // release the store lock before the child process opens it

    let db = db.display().to_string();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["path", "A:1", "--db", &db])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("cycle"), "stderr: {stderr}");

    Command::cargo_bin("figmog")
        .unwrap()
        .args(["stats", "--db", &db])
        .assert()
        .success();
}

/// The same hand-upserted 2-node mutual-parent construction as
/// `parent_cycle_does_not_hang_path_or_stats` above also derives a
/// `children`-multimap cycle (`store::child_edge` feeds `children` off
/// each node's own `parent_id`, so A:1's parent B:1 and B:1's parent A:1
/// produce mutual `children` edges too) — `--under`'s BFS
/// (`query::scope_ids`) must not hang on it, same "runs inside `figmog
/// serve`'s single-threaded loop" stakes as the parent-cycle case.
#[test]
fn children_cycle_does_not_hang_under_scoping() {
    use figmog::model::{Id, NodeRec, Rec};

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let mut st = figmog::open_store!(&db);
    let node = |id: &str, parent: &str| NodeRec {
        id: id.into(),
        parent_id: Some(parent.into()),
        child_index: 0,
        page_id: "0:1".into(),
        node_type: "FRAME".into(),
        name: id.into(),
        visible: true,
        text: None,
        component_id: None,
        component_properties: vec![],
        property_definitions: None,
        style_refs: vec![],
        bound_variables: vec![],
        abs_bounds: None,
        raw: "{}".into(),
    };
    st.wtx(|tx| {
        tx.upsert(&Id::Node("A:1".into()), &Rec::Node(node("A:1", "B:1")));
        tx.upsert(&Id::Node("B:1".into()), &Rec::Node(node("B:1", "A:1")));
    });
    drop(st); // release the store lock before the child process opens it

    let db = db.display().to_string();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["find", "--type", "FRAME", "--under", "A:1", "--db", &db])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["A:1", "B:1"],
        "BFS terminates and still finds both cyclic nodes"
    );
}

#[test]
fn import_variables_upgrades_vars_to_authoritative() {
    let (dir, db) = fixture_db();
    let export = dir.path().join("vars.json");
    std::fs::write(&export, include_str!("fixtures/variables-export.json")).unwrap();

    Command::cargo_bin("figmog")
        .unwrap()
        .args(["import-variables", export.to_str().unwrap(), "--db", &db])
        .assert()
        .success();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["vars", "--db", &db])
        .assert()
        .success();
    let vars: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let v100 = vars
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["variable_id"] == "VariableID:100")
        .unwrap();
    assert_eq!(v100["source"], "imported");
    assert_eq!(v100["name"], "color/surface/primary");
    assert_eq!(v100["collection"], "colors");
    assert_eq!(v100["values_by_mode"]["light"]["r"], 0.06);
    // inference detail still present alongside
    assert_eq!(v100["sites"][0][0], "1:1");
}

/// M6 (spec §4 debt ledger): `figmog import-variables`' JSON output shape
/// is exactly `{"imported": N}`, counting only `Variable` records —
/// `variables-export.json` has 3 variables across 2 collections, so
/// `imported` must be 3, not 5.
#[test]
fn import_variables_output_shape_is_the_variable_count_alone() {
    let (dir, db) = fixture_db();
    let export = dir.path().join("vars.json");
    std::fs::write(&export, include_str!("fixtures/variables-export.json")).unwrap();

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["import-variables", export.to_str().unwrap(), "--db", &db])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v, serde_json::json!({"imported": 3}));
}

/// M6 (spec §4 debt ledger): `figmog tools`' JSON output shape — an array
/// with one `{name, source, cacheable}` row per tool, nothing more.
/// `--no-upstream` keeps this offline and deterministic: exactly the 20
/// local `figmog_*` tools (v0.0.2 §2 added `figmog_subtree`), every one
/// `source: "local"`.
#[test]
fn tools_output_shape_is_name_source_cacheable_rows() {
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["tools", "--no-upstream"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let rows = v.as_array().expect("tools output is a JSON array");
    assert_eq!(rows.len(), 20, "20 local figmog_* tools, no upstream");
    for row in rows {
        let obj = row.as_object().expect("each row is a JSON object");
        let keys: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["name", "source", "cacheable"].into_iter().collect(),
            "row has exactly name/source/cacheable: {row}"
        );
        assert_eq!(obj["source"], serde_json::json!("local"));
        assert!(obj["name"].as_str().unwrap().starts_with("figmog_"));
    }
}

/// M5 (spec §4 debt ledger): `figmog tools` never opens the store, so it
/// must not require a resolved mirror — unlike every other command, it
/// works with no `.figmog/current` and no `--db` at all.
#[test]
fn tools_works_with_no_established_mirror() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("figmog")
        .unwrap()
        .current_dir(dir.path())
        .args(["tools", "--no-upstream"])
        .assert()
        .success();
    assert!(
        !dir.path().join(".figmog").exists(),
        "figmog tools must not create or require a mirror"
    );
}

#[test]
fn failed_pull_does_not_persist_current_or_create_store() {
    let dir = tempfile::tempdir().unwrap();

    // A well-formed-looking key (>=10 alnum chars) with no FIGMA_TOKEN set:
    // the network pull fails before ever touching the store or writing
    // `.figmog/current`.
    Command::cargo_bin("figmog")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("FIGMA_TOKEN")
        .args(["pull", "garbagekey123456"])
        .assert()
        .failure()
        .code(1);

    assert!(
        !dir.path().join(".figmog").exists(),
        "a failed pull must not create `.figmog` (no current key, no store dir)"
    );

    // A subsequent read command still reports no mirror — not a stale or
    // bogus one — and doesn't leave behind an empty store dir either.
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .current_dir(dir.path())
        .args(["status"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("no mirror here"), "stderr: {stderr}");
    assert!(!dir.path().join(".figmog").exists());
}

#[test]
fn call_figmog_sync_fails_cleanly_not_panicking() {
    // Regression test: `cmd_call`'s `figmog_sync` branch used to open its
    // own store handle unconditionally, then delegate to `do_pull`, which
    // opens the *same* path again — fjall allows only one open handle per
    // store per process, so a real sync would panic on the second open's
    // file lock (after the Tier-1 fetch was already spent). `cmd_call` now
    // checks for `figmog_sync` and returns before ever opening its own
    // handle, so this call — which fails during `do_pull`'s own key
    // resolution, since `--db` alone establishes no file key — has to fail
    // cleanly (exit 1, one JSON stderr line), never panic, for the fix to
    // hold: a panic would print a backtrace banner and a different exit
    // status instead.
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .env_remove("FIGMA_TOKEN")
        .args(["call", "figmog_sync", "--db", &db])
        .assert()
        .failure()
        .code(1);
    let stderr = out.get_output().stderr.clone();
    let v: serde_json::Value = serde_json::from_slice(&stderr).unwrap_or_else(|e| {
        panic!(
            "stderr not JSON: {e}\nstderr: {}",
            String::from_utf8_lossy(&stderr)
        )
    });
    assert!(
        v["error"].is_string(),
        "expected a clean {{\"error\": ...}} stderr, got: {v}"
    );
    assert!(
        !v["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("panic"),
        "must not panic: {v}"
    );
}

#[test]
fn cli_pull_evicts_stale_cache_rows_on_version_change() {
    // I4: eviction lives inside `do_pull` itself (not just `figmog
    // serve`'s two inline blocks), so it covers both `figmog pull` and
    // `figmog call figmog_sync`, which delegate to `do_pull`. Exercised
    // here through the actual `figmog pull` CLI
    // command (the store handle used to hand-insert the cache row is
    // dropped before each CLI invocation — fjall allows only one open
    // handle per store per process).
    use figmog::cache;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let db_str = db.display().to_string();

    let response_v1 = dir.path().join("v1.json");
    std::fs::write(
        &response_v1,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response_v1.to_str().unwrap(),
            "--db",
            &db_str,
        ])
        .assert()
        .success();

    // Hand-store a proxy_cache row tagged at v1's version ("100"), scoped
    // so the store handle closes before the next `figmog pull` subprocess
    // opens its own.
    {
        let mut st = figmog::open_store!(&db);
        cache::store(
            &mut st,
            "get_code",
            "{}",
            "100",
            &serde_json::json!({"cached": true}),
        )
        .unwrap();
        let hit = st.rtx(|(_, _, _, _, _, _, _, cache_reader, _)| {
            cache::lookup(&cache_reader, "get_code", "{}", "100")
        });
        assert!(
            hit.is_some(),
            "sanity: the hand-stored row must be readable before the v2 pull"
        );
    }

    // v2 (version "101") via `figmog pull` — this is the version-changing
    // pull that must sweep the stale row.
    let response_v2 = dir.path().join("v2.json");
    std::fs::write(
        &response_v2,
        serde_json::to_string(&common::fixture_v2()).unwrap(),
    )
    .unwrap();
    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response_v2.to_str().unwrap(),
            "--db",
            &db_str,
        ])
        .assert()
        .success();

    let st = figmog::open_store!(&db);
    let evicted = st.rtx(|(_, _, _, _, _, _, _, cache_reader, _)| {
        cache::lookup(&cache_reader, "get_code", "{}", "100")
    });
    assert_eq!(
        evicted, None,
        "the v1-tagged cache row must be evicted by the v2 `figmog pull`"
    );
}

/// I-1: `figmog`'s stdout write must tolerate a reader that closes
/// mid-write — piped into `head`, a killed consumer, a closed terminal —
/// with a clean exit 0, not a `println!` panic and not the ordinary
/// exit-1 error path (the write was never at fault; its consumer left).
/// Reproduced by asking `figmog get` for one node whose raw JSON carries a
/// ~4MB `characters` string with no embedded newlines — comfortably
/// larger than any platform's default pipe buffer (16KB-1MB), so the
/// write can't complete in one buffered burst before a reader has a
/// chance to go away mid-stream. Manual `std::process::Command` (not
/// `assert_cmd`, which reads to completion) so the read end can be
/// dropped after only a few bytes while the child almost certainly still
/// has megabytes left to write.
#[test]
fn write_json_survives_a_reader_that_closes_mid_write() {
    use std::io::Read;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let mut doc = common::fixture_v1();
    // Node 1:2 ("Title", a TEXT node under Page 1 -> Hero -> Title).
    // `figmog get` dumps a node's raw JSON verbatim, so a huge string
    // value on it becomes one long output line with no embedded newline —
    // forcing a single sustained write rather than many small,
    // line-buffered ones. A made-up field name (not `characters`) so it
    // never reaches the BM25 text index — a single non-whitespace "word"
    // that size would itself blow past the store's own key-length limit
    // there; this way it's purely payload, round-tripped verbatim through
    // `raw` untouched by any pipeline branch.
    doc["document"]["children"][0]["children"][0]["children"][0]["hugePayload"] =
        serde_json::Value::String("x".repeat(4_000_000));
    let response = dir.path().join("resp.json");
    std::fs::write(&response, serde_json::to_string(&doc).unwrap()).unwrap();
    let db = dir.path().join("db");

    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .assert()
        .success();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_figmog"))
        .args(["get", "1:2", "--db", db.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = child.stdout.take().unwrap();
    let mut prefix = [0u8; 64];
    stdout
        .read_exact(&mut prefix)
        .expect("child should have written at least a small prefix");
    drop(stdout); // close our read end — the child's next write should EPIPE

    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "a vanished reader must be a clean exit 0, not a panic (101) or an error exit"
    );
}

/// JSON is the only output mode now (spec §4): the global `--json` flag no
/// longer exists, so passing it must fail clap's own argument parsing
/// (unknown flag) rather than being silently accepted.
#[test]
fn json_flag_is_rejected_as_unknown() {
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["status", "--db", &db, "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("--json") || stderr.to_lowercase().contains("unexpected argument"),
        "expected clap to reject the removed --json flag: {stderr}"
    );
}

// ---- v0.0.2 §2: subtree dump ----

#[test]
fn dump_full_raw_json_nested_recursively_with_depth_and_fields_projection() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    // Unbounded depth (default): full raw JSON, children nested in
    // child-index order.
    let dump = run(&["dump", "1:1"]);
    assert_eq!(dump["id"], "1:1");
    assert_eq!(dump["layoutMode"], "VERTICAL"); // raw field preserved verbatim
    let kids = dump["children"].as_array().unwrap();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0]["id"], "1:2"); // Title, numeric child order
    assert_eq!(kids[0]["characters"], "Welcome to the garden");
    assert_eq!(kids[1]["id"], "1:3"); // Button

    // depth 0: the node itself only; `children` is still present, just empty.
    let dump0 = run(&["dump", "1:1", "--depth", "0"]);
    assert_eq!(dump0["children"], serde_json::json!([]));
    assert_eq!(dump0["layoutMode"], "VERTICAL");

    // depth 1 from the page: one level down, grandchildren cut off.
    let dump1 = run(&["dump", "0:1", "--depth", "1"]);
    let page_kids = dump1["children"].as_array().unwrap();
    assert_eq!(page_kids.len(), 2); // Hero (1:1), Old badge (1:9)
    assert_eq!(page_kids[0]["id"], "1:1");
    assert_eq!(
        page_kids[0]["children"],
        serde_json::json!([]),
        "depth exhausted before Hero's own children"
    );

    // fields projection: only requested fields plus the always-kept set
    // (id/name/type/children) survive.
    let projected = run(&["dump", "1:1", "--fields", "layoutMode"]);
    let keys: std::collections::BTreeSet<&str> = projected
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["id", "name", "type", "children", "layoutMode"]
            .into_iter()
            .collect(),
        "fields projection keeps id/name/type/children always, plus requested fields"
    );
    assert_eq!(
        projected["children"][0]["layoutMode"],
        serde_json::Value::Null
    );

    // Unknown field names are silently absent, not an error.
    let unknown = run(&["dump", "1:1", "--fields", "notAField"]);
    assert!(!unknown.as_object().unwrap().contains_key("notAField"));
    assert_eq!(unknown["id"], "1:1"); // still always-kept
}

#[test]
fn dump_unknown_node_fails_cleanly() {
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["dump", "99:99", "--db", &db])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("99:99"), "stderr: {stderr}");
}

// ---- v0.0.2 §3: --under subtree scoping ----

#[test]
fn under_scopes_find_text_search_where_and_intersects_with_page() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    // find --under: COMPONENT nodes live on page 0:2, outside 1:1's subtree.
    let none = run(&["find", "--type", "COMPONENT", "--under", "1:1"]);
    assert_eq!(none.as_array().unwrap().len(), 0);

    // --under is inclusive of the scope root itself.
    let inclusive = run(&["find", "--type", "FRAME", "--under", "1:1"]);
    assert_eq!(inclusive.as_array().unwrap().len(), 1);
    assert_eq!(inclusive[0]["id"], "1:1");

    // text --under narrows to one page's subtree.
    let text_under = run(&["text", "--under", "1:1"]);
    assert_eq!(text_under.as_array().unwrap().len(), 1);
    assert_eq!(text_under[0]["id"], "1:2");
    let text_elsewhere = run(&["text", "--under", "2:1"]); // Button component set: no TEXT nodes
    assert_eq!(text_elsewhere.as_array().unwrap().len(), 0);

    // search --under.
    let search_under = run(&["search", "garden", "--under", "1:1"]);
    assert_eq!(search_under[0]["id"], "1:2");
    let search_outside = run(&["search", "garden", "--under", "0:2"]);
    assert_eq!(search_outside.as_array().unwrap().len(), 0);

    // where --under.
    let where_under = run(&[
        "where",
        "--pointer",
        "/layoutMode",
        "--equals",
        "VERTICAL",
        "--under",
        "1:1",
    ]);
    assert_eq!(where_under.as_array().unwrap().len(), 1);
    let where_outside = run(&[
        "where",
        "--pointer",
        "/layoutMode",
        "--equals",
        "VERTICAL",
        "--under",
        "0:2",
    ]);
    assert_eq!(where_outside.as_array().unwrap().len(), 0);

    // --under intersects with --page: page 0:2's TEXT nodes scoped under
    // page 0:1's subtree is the empty intersection.
    let intersected = run(&["find", "--type", "TEXT", "--page", "0:2", "--under", "0:1"]);
    assert_eq!(
        intersected.as_array().unwrap().len(),
        0,
        "page and under compose by intersection"
    );
}

#[test]
fn under_unknown_id_errors_cleanly() {
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["find", "--type", "TEXT", "--under", "99:99", "--db", &db])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("99:99"), "stderr: {stderr}");
}

/// Review fix I1: `search --under` must rank the *entire* matching corpus
/// before truncating to `limit`, not truncate to the global top-`limit`
/// and filter afterward — otherwise a tight limit under a scope that
/// contains a real (if globally lower-ranked) match silently returns
/// nothing. Fixture: five short out-of-scope TEXT nodes named exactly
/// "garden" (BM25 favors them — an exact, single-term document scores
/// higher than a longer one containing the term once) rank above the one
/// in-scope node whose text merely *contains* "garden" among other words.
#[test]
fn search_under_ranks_the_full_corpus_before_truncating_to_limit() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    let fixture = serde_json::json!({
        "name": "SearchUnderFixture",
        "version": "1",
        "lastModified": "2026-08-17T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [
                { "id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                    { "id": "1:1", "name": "Scope", "type": "FRAME", "children": [
                        { "id": "1:2", "name": "Info", "type": "TEXT",
                          "characters": "A lovely garden path unfolds among tall trees",
                          "children": [] }
                    ] }
                ] },
                { "id": "0:2", "name": "Page 2", "type": "CANVAS", "children": [
                    { "id": "2:1", "name": "garden", "type": "TEXT", "children": [] },
                    { "id": "2:2", "name": "garden", "type": "TEXT", "children": [] },
                    { "id": "2:3", "name": "garden", "type": "TEXT", "children": [] },
                    { "id": "2:4", "name": "garden", "type": "TEXT", "children": [] },
                    { "id": "2:5", "name": "garden", "type": "TEXT", "children": [] }
                ] }
            ]
        },
        "components": {}, "componentSets": {}, "styles": {}
    });
    std::fs::write(&response, serde_json::to_string(&fixture).unwrap()).unwrap();
    let db = dir.path().join("db").display().to_string();
    Command::cargo_bin("figmog")
        .unwrap()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            &db,
        ])
        .assert()
        .success();

    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    // Premise check: unscoped top-1 is an out-of-scope short match, not
    // the in-scope one — otherwise this fixture doesn't reproduce I1.
    let unscoped = run(&["search", "garden", "-n", "1"]);
    assert_ne!(
        unscoped[0]["id"], "1:2",
        "premise: the in-scope match must rank below the out-of-scope ones globally"
    );

    // The reviewer's exact repro shape: a tight limit under a scope whose
    // only match is globally outranked must still return it, not [].
    let scoped = run(&["search", "garden", "-n", "1", "--under", "1:1"]);
    let ids: Vec<&str> = scoped
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["1:2"],
        "scoped search ranks the full corpus before truncating"
    );

    // A wider limit that still fits inside the scope: same single hit,
    // nothing else under 1:1 matches "garden".
    let scoped_wide = run(&["search", "garden", "-n", "5", "--under", "1:1"]);
    assert_eq!(scoped_wide.as_array().unwrap().len(), 1);
    assert_eq!(scoped_wide[0]["id"], "1:2");
}

// ---- v0.0.2 §6: --resolve-vars ----

#[test]
fn resolve_vars_marks_bindings_unresolved_with_no_variables_table() {
    let (_dir, db) = fixture_db();

    // No import: both of 1:1's bound variables (fill color, paddingLeft)
    // have no entry in the (empty) variables table.
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "1:1", "--resolve-vars", "--db", &db])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let resolved = v["resolved_variables"].as_array().unwrap();
    assert_eq!(resolved.len(), 2);
    for entry in resolved {
        assert_eq!(entry["source"], "unresolved");
        assert!(
            entry["variable_id"]
                .as_str()
                .unwrap()
                .starts_with("VariableID:")
        );
        assert!(entry.get("variable_name").is_none());
    }
}

#[test]
fn resolve_vars_resolves_imported_variables_on_get_dump_and_styles_values() {
    let (dir, db) = fixture_db();
    let export = dir.path().join("vars.json");
    std::fs::write(&export, include_str!("fixtures/variables-export.json")).unwrap();
    Command::cargo_bin("figmog")
        .unwrap()
        .args(["import-variables", export.to_str().unwrap(), "--db", &db])
        .assert()
        .success();

    // `get --resolve-vars`: names + per-mode values from the imported table.
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "1:1", "--resolve-vars", "--db", &db])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let resolved = v["resolved_variables"].as_array().unwrap();
    assert_eq!(resolved.len(), 2);
    let color = resolved
        .iter()
        .find(|e| e["variable_id"] == "VariableID:100")
        .expect("fill color binding present");
    assert_eq!(color["variable_name"], "color/surface/primary");
    assert_eq!(color["values_by_mode"]["light"]["r"], 0.06);
    assert_eq!(color["values_by_mode"]["dark"]["type"], "VARIABLE_ALIAS");
    let padding = resolved
        .iter()
        .find(|e| e["variable_id"] == "VariableID:200")
        .expect("paddingLeft binding present");
    assert_eq!(padding["variable_name"], "space/md");
    assert_eq!(padding["values_by_mode"]["default"], 16.0);

    // `dump --resolve-vars`: same annotation, applied recursively.
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["dump", "1:1", "--resolve-vars", "--db", &db])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["resolved_variables"].as_array().unwrap().len(), 2);

    // `styles --values --resolve-vars`: FILL style S:1's definition comes
    // from consumer 1:1, whose /fills/0/color binding is VariableID:100 —
    // paddingLeft (outside /fills) must not leak into this style's list.
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["styles", "--values", "--resolve-vars", "--db", &db])
        .assert()
        .success();
    let styles: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let fill_style = styles
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["style_id"] == "S:1")
        .expect("FILL style S:1 present");
    let fill_resolved = fill_style["resolved_variables"].as_array().unwrap();
    assert_eq!(fill_resolved.len(), 1);
    assert_eq!(fill_resolved[0]["variable_id"], "VariableID:100");
    assert_eq!(fill_resolved[0]["variable_name"], "color/surface/primary");
}

// ---- v0.0.2 §2b: URL-addressed nodes ----

#[test]
fn get_dump_and_path_accept_a_full_figma_url_as_the_node_id() {
    let (_dir, db) = fixture_db();
    let url = "https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/Fixture?node-id=1-1&t=abc-1";

    let bare = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "1:1", "--db", &db])
        .assert()
        .success();
    let bare: serde_json::Value = serde_json::from_slice(&bare.get_output().stdout).unwrap();

    let via_url = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", url, "--db", &db])
        .assert()
        .success();
    let via_url: serde_json::Value = serde_json::from_slice(&via_url.get_output().stdout).unwrap();
    assert_eq!(
        bare, via_url,
        "a frame URL resolves to the same node as its bare id"
    );

    let dump_via_url = Command::cargo_bin("figmog")
        .unwrap()
        .args(["dump", url, "--depth", "0", "--db", &db])
        .assert()
        .success();
    let dump_via_url: serde_json::Value =
        serde_json::from_slice(&dump_via_url.get_output().stdout).unwrap();
    assert_eq!(dump_via_url["id"], "1:1");

    let path_via_url = Command::cargo_bin("figmog")
        .unwrap()
        .args(["path", url, "--db", &db])
        .assert()
        .success();
    let path_via_url: serde_json::Value =
        serde_json::from_slice(&path_via_url.get_output().stdout).unwrap();
    let ids: Vec<&str> = path_via_url
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["0:0", "0:1", "1:1"]);
}

/// Review fix I2: `--page` is a node-id argument too (a page id), and
/// hadn't been routed through the same URL-aware normalization as
/// `id`/`under`/`target` — a pasted Figma URL silently matched nothing.
#[test]
fn page_argument_accepts_a_full_figma_url() {
    let (_dir, db) = fixture_db();
    let run = |args: &[&str]| {
        let out = Command::cargo_bin("figmog")
            .unwrap()
            .args(args)
            .args(["--db", &db])
            .assert()
            .success();
        serde_json::from_slice::<serde_json::Value>(&out.get_output().stdout).unwrap()
    };

    let bare = run(&["find", "--type", "TEXT", "--page", "0:1"]);
    let url = "https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/Fixture?node-id=0-1&t=abc-1";
    let via_url = run(&["find", "--type", "TEXT", "--page", url]);
    assert_eq!(
        bare, via_url,
        "a page URL matches the same rows as its bare id"
    );
    assert_eq!(bare.as_array().unwrap().len(), 1);
    assert_eq!(bare[0]["id"], "1:2");
}
