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
        let hit = st.rtx(|(_, _, _, _, _, _, _, cache_reader)| {
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
    let evicted = st.rtx(|(_, _, _, _, _, _, _, cache_reader)| {
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
