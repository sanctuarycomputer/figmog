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
            "--json",
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
            "--json",
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
            .args(["--db", &db, "--json"])
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
fn get_unknown_node_json_error_is_json_on_stderr() {
    let (_dir, db) = fixture_db();
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["get", "99:99", "--db", &db, "--json"])
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
            .args(["--db", &db, "--json"])
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
            .args(["--db", &db, "--json"])
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
        .args(["stats", "--db", &db, "--json"])
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
        .args(["vars", "--db", &db, "--json"])
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
    // cleanly (exit 1, one plain stderr line), never panic, for the fix to
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
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.starts_with("figmog:"),
        "expected a clean `figmog: ...` error, got: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("panic"),
        "must not panic: {stderr}"
    );
}

#[test]
fn cli_pull_evicts_stale_cache_rows_on_version_change() {
    // I4: eviction lives inside `do_pull` itself (not just `figmog
    // serve`'s two inline blocks), so it covers `figmog pull`, `figmog
    // watch`, and `figmog call figmog_sync` — all three delegate to
    // `do_pull`. Exercised here through the actual `figmog pull` CLI
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
        );
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

/// End-to-end smoke for `figmog bench` (build design §13), synthetic-mode
/// only (no `FIGMA_TOKEN` in CI): a small corpus/call count keeps this fast
/// while still exercising every phase — corpus generation, cold sync,
/// no-churn re-pull, and a real MCP `serve` child driven over stdio.
#[test]
fn bench_e2e_synthetic_json_report() {
    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["bench", "--nodes", "300", "--calls", "60", "--json"])
        .assert()
        .success();
    let output = out.get_output();

    // stdout purity: --json means exactly one JSON object, nothing else.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout was not exactly one JSON object: {e}\nstdout: {stdout}")
    });

    assert_eq!(v["source"], serde_json::json!("synthetic"));
    assert_eq!(v["corpus"]["nodes"], serde_json::json!(300));
    assert_eq!(v["repull"]["churn_zero"], serde_json::json!(true));
    assert_eq!(v["load"]["total_calls"], serde_json::json!(60));
    assert!(
        v["api"].is_null(),
        "synthetic mode never runs the API comparison phase"
    );

    let per_tool = v["load"]["per_tool"].as_array().expect("per_tool array");
    assert!(!per_tool.is_empty());
    for tool in per_tool {
        let p50 = tool["p50_ms"].as_f64().expect("p50_ms is a number");
        assert!(p50 >= 0.0, "p50 should be non-negative: {tool}");
    }
}

/// `--interactive` is a human-only REPL (build design §13): combined with
/// `--json` it's a usage error, not a silent pick-one. Exit 1, nothing on
/// stdout, and (since `--json` was set) the error is JSON on stderr —
/// `cli::run`'s top-level error handler emits `{"error": …}` there when
/// `cli.json` is true, matching every other command's `--json` error
/// convention.
#[test]
fn bench_interactive_and_json_is_a_usage_error() {
    let assert = Command::cargo_bin("figmog")
        .unwrap()
        .args(["bench", "--nodes", "300", "--interactive", "--json"])
        .assert()
        .failure()
        .code(1);
    let output = assert.get_output();

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on a usage error: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap_or_else(|e| {
        panic!("stderr was not exactly one JSON object: {e}\nstderr: {stderr}")
    });
    let msg = v["error"]
        .as_str()
        .expect("stderr JSON has an `error` string field");
    assert!(
        msg.contains("--interactive") && msg.contains("--json"),
        "expected the error to name both conflicting flags: {msg:?}"
    );
}

/// End-to-end smoke for `figmog bench --interactive` (build design §13
/// "Interactive mode"), driven non-interactively by piping a command
/// script into stdin — this is exactly how CI (a non-TTY pipe) exercises
/// it, and it's also the scenario the "no ANSI in plain mode" guarantee
/// matters for.
#[test]
fn bench_interactive_e2e_scripted_session_is_plain_and_clean() {
    let script = "help\nstats\nsearch garden\nrun 20\nreport\nquit\n";

    let out = Command::cargo_bin("figmog")
        .unwrap()
        .args(["bench", "--nodes", "300", "--interactive"])
        .write_stdin(script)
        .assert()
        .success();
    let output = out.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.as_bytes().contains(&0x1b),
        "non-TTY stdout must contain zero ANSI escape bytes:\n{stdout}"
    );

    assert!(
        stdout.contains("figmog_search"),
        "expected a per-request line naming figmog_search:\n{stdout}"
    );

    // Sequence numbers are session-wide, not per-command: `stats` fires
    // #1, `search garden` fires #2, so `run 20`'s 20 numbered per-request
    // lines are #3 through #22. `#1` and `#20` are both still present
    // somewhere in that combined stream — the first from `stats`, the
    // second from partway through the burst — which is enough to confirm
    // both the pre-burst call and the burst itself actually fired.
    for n in [1, 20] {
        let needle = format!("#{n:>4}");
        assert!(
            stdout.contains(&needle),
            "expected a numbered line #{n} ({needle:?}) somewhere in the session:\n{stdout}"
        );
    }

    // A report table (headers shared by both `run`'s burst table and
    // `report`'s cumulative one).
    assert!(
        stdout.contains("p50 (ms)") && stdout.contains("p95 (ms)"),
        "expected a percentile report table:\n{stdout}"
    );

    // `help`'s command list and a clean `quit`.
    assert!(
        stdout.contains("commands:"),
        "expected `help`'s output:\n{stdout}"
    );
}
