//! `figmog bench` — the self-contained load-test demo (build design §13).
//!
//! Four (five, in real-file mode) timed phases against one temp store:
//!
//! 1. **Corpus** — a deterministic synthetic Figma file (seeded LCG, no
//!    wall-clock) or, given a file key/URL, one real Tier-1 fetch.
//! 2. **Cold sync** — `flatten_file` + `store::sync` into a fresh store.
//! 3. **No-churn re-pull** — `sync` the identical (in-memory, not
//!    re-fetched) data again; the engine's headline invariant, timed and
//!    asserted in code.
//! 4. **Serve load** — spawn the real `figmog serve --no-upstream
//!    --no-watch` binary and drive it over its real stdio pipe with a
//!    fixed rotating tool-call mix, timing write→response-line per call.
//! 5. **API comparison** (real-file mode only, unless `--skip-api`) — a
//!    handful of native `GET /nodes` calls timed the same way, so the
//!    report can show figmog's local reads next to Figma's rate-limited
//!    API side by side.
//!
//! Synthetic and real-file mode share one code path from flatten onward:
//! the load-test's query mix (search words, node ids, the instances
//! target) is always *derived* from the flattened records, never
//! hardcoded — see [`derive_query_pool`].
//!
//! Phases 1-3 (`prepare`) are shared by two entry points: [`run`] (this
//! module's automated one-shot phases 4/5 above) and [`run_interactive`]
//! (`--interactive`, build design §13 "Interactive mode"), which spawns
//! the same serve child via [`BenchSession`] and hands it to
//! [`crate::repl::run`] for a live REPL instead of the automated load/API
//! phases. [`BenchSession::fire`] — one raw `tools/call` frame, timed — is
//! the primitive both the one-shot load phase and the REPL drive.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::cli::open_store_checked;
use crate::flatten::{Flattened, flatten_file};
use crate::model::{Id, Rec};
use crate::store::{self, collect_sweepable};

// ---- deterministic LCG ----

/// Fixed seed and constants: no wall-clock, no `rand` dep — the same
/// `--nodes` always yields byte-identical corpus JSON (spec §13).
const LCG_SEED: u64 = 0x243F_6A88_85A3_08D3;
const LCG_MUL: u64 = 6364136223846793005;
const LCG_INC: u64 = 1442695040888963407;

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(LCG_MUL).wrapping_add(LCG_INC);
        self.0
    }
    /// Uniform pick in `0..n`. Panics if `n == 0` (never called that way
    /// below — every call site checks non-emptiness first).
    fn next_range(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Fixed 64-word pool: plausible design vocabulary so BM25 search has real
/// queries against the synthetic corpus (spec §13).
const WORDS: [&str; 64] = [
    "Button",
    "Label",
    "Header",
    "Footer",
    "Card",
    "Icon",
    "Input",
    "Modal",
    "Nav",
    "Menu",
    "Title",
    "Subtitle",
    "Body",
    "Caption",
    "Badge",
    "Avatar",
    "Toggle",
    "Slider",
    "Tab",
    "Panel",
    "Sidebar",
    "Toolbar",
    "Dialog",
    "Tooltip",
    "Dropdown",
    "Checkbox",
    "Radio",
    "Switch",
    "Field",
    "Form",
    "Table",
    "Row",
    "Column",
    "Grid",
    "List",
    "Item",
    "Link",
    "Divider",
    "Spacer",
    "Container",
    "Wrapper",
    "Section",
    "Hero",
    "Banner",
    "Alert",
    "Notification",
    "Progress",
    "Spinner",
    "Loader",
    "Chip",
    "Tag",
    "Pill",
    "Breadcrumb",
    "Pagination",
    "Stepper",
    "Accordion",
    "Carousel",
    "Gallery",
    "Thumbnail",
    "Preview",
    "Overlay",
    "Backdrop",
    "Popover",
    "Snackbar",
];

/// Grid position -> `absoluteBoundingBox`. Every generated node calls this
/// once, in emission order, so the whole file lays out on a simple grid.
fn grid_bounds(pos: &mut u64) -> Value {
    let p = *pos;
    *pos += 1;
    let col = p % 20;
    let row = p / 20;
    json!({
        "x": (col * 100) as f64,
        "y": (row * 100) as f64,
        "width": 90.0,
        "height": 90.0,
    })
}

// ---- corpus generation ----

/// Generate a deterministic synthetic `GET /v1/files/:key`-shaped response
/// with exactly `nodes` flattened node records: pages of auto-layout
/// frames (~1 page per 250 nodes), TEXT children drawn from [`WORDS`], one
/// "Button" `COMPONENT_SET` (two variants) on the first page, INSTANCE
/// nodes referencing it (every ~20th frame child), two styles referenced
/// by ~every frame/TEXT node, and `boundVariables` fill bindings on ~every
/// 10th frame. Pure and wall-clock free: two calls with the same `nodes`
/// produce byte-identical JSON.
pub fn generate_corpus(nodes: usize) -> Value {
    let mut rng = Lcg::new(LCG_SEED);
    let mut pos: u64 = 0;

    let doc_bounds = grid_bounds(&mut pos);
    let mut components = serde_json::Map::new();
    let mut component_sets = serde_json::Map::new();
    let styles = json!({
        "S:1": {"key": "sk1", "name": "Bench/Fill", "styleType": "FILL", "description": "", "remote": false},
        "S:2": {"key": "sk2", "name": "Bench/Text", "styleType": "TEXT", "description": "", "remote": false},
    });

    const VARIABLE_IDS: [&str; 3] = ["VariableID:100", "VariableID:101", "VariableID:102"];
    let mut variants: Vec<(String, &'static str)> = Vec::new(); // (component node id, state label)

    let mut pages: Vec<Value> = Vec::new();
    let mut remaining = nodes.saturating_sub(1); // minus DOCUMENT
    let mut page_num = 0usize;
    let mut global_frame_idx = 0usize;
    let mut global_child_idx = 0usize;

    while remaining > 0 {
        page_num += 1;
        let mut local = 0usize; // per-page "p:i" id counter
        remaining -= 1; // the CANVAS node itself
        let page_id = format!("{page_num}:0");
        let mut page_children: Vec<Value> = Vec::new();

        if page_num == 1 && remaining >= 3 {
            local += 1;
            let set_id = format!("{page_num}:{local}");
            local += 1;
            let variant1_id = format!("{page_num}:{local}");
            local += 1;
            let variant2_id = format!("{page_num}:{local}");
            remaining -= 3;

            variants.push((variant1_id.clone(), "Default"));
            variants.push((variant2_id.clone(), "Hover"));

            component_sets.insert(
                set_id.clone(),
                json!({"key": "keyset-button", "name": "Button", "description": "", "remote": false}),
            );
            components.insert(
                variant1_id.clone(),
                json!({"key": "key-button-default", "name": "State=Default", "description": "", "componentSetId": set_id, "remote": false}),
            );
            components.insert(
                variant2_id.clone(),
                json!({"key": "key-button-hover", "name": "State=Hover", "description": "", "componentSetId": set_id, "remote": false}),
            );

            page_children.push(json!({
                "id": set_id,
                "name": "Button",
                "type": "COMPONENT_SET",
                "absoluteBoundingBox": grid_bounds(&mut pos),
                "componentPropertyDefinitions": {
                    "State": {"type": "VARIANT", "defaultValue": "Default", "variantOptions": ["Default", "Hover"]}
                },
                "children": [
                    {
                        "id": variant1_id,
                        "name": "State=Default",
                        "type": "COMPONENT",
                        "absoluteBoundingBox": grid_bounds(&mut pos),
                        "children": [],
                    },
                    {
                        "id": variant2_id,
                        "name": "State=Hover",
                        "type": "COMPONENT",
                        "absoluteBoundingBox": grid_bounds(&mut pos),
                        "children": [],
                    },
                ],
            }));
        }

        let mut page_budget = page_children.len();
        while remaining > 0 && page_budget < 250 {
            local += 1;
            let frame_id = format!("{page_num}:{local}");
            remaining -= 1;
            page_budget += 1;
            global_frame_idx += 1;

            let want = 3 + rng.next_range(4); // 3..=6 children
            let child_count = want.min(remaining);

            let mut children: Vec<Value> = Vec::with_capacity(child_count);
            for _ in 0..child_count {
                local += 1;
                let child_id = format!("{page_num}:{local}");
                remaining -= 1;
                page_budget += 1;
                global_child_idx += 1;

                if global_child_idx.is_multiple_of(20) && !variants.is_empty() {
                    let (comp_id, state) = &variants[rng.next_range(variants.len())];
                    children.push(json!({
                        "id": child_id,
                        "name": "Button",
                        "type": "INSTANCE",
                        "componentId": comp_id,
                        "componentProperties": {
                            "State": {"value": state, "type": "VARIANT"}
                        },
                        "absoluteBoundingBox": grid_bounds(&mut pos),
                        "children": [],
                    }));
                } else {
                    let word_count = 4 + rng.next_range(5); // 4..=8 words
                    let text = (0..word_count)
                        .map(|_| WORDS[rng.next_range(WORDS.len())])
                        .collect::<Vec<_>>()
                        .join(" ");
                    children.push(json!({
                        "id": child_id,
                        "name": text,
                        "type": "TEXT",
                        "characters": text,
                        "styles": {"text": "S:2"},
                        "absoluteBoundingBox": grid_bounds(&mut pos),
                        "children": [],
                    }));
                }
            }

            let mut frame = json!({
                "id": frame_id,
                "name": format!("{} Frame", WORDS[rng.next_range(WORDS.len())]),
                "type": "FRAME",
                "layoutMode": "VERTICAL",
                "styles": {"fill": "S:1"},
                "absoluteBoundingBox": grid_bounds(&mut pos),
                "children": children,
            });
            if global_frame_idx.is_multiple_of(10) {
                let var_id = VARIABLE_IDS[rng.next_range(VARIABLE_IDS.len())];
                frame["fills"] = json!([{
                    "type": "SOLID",
                    "color": {"r": 0.2, "g": 0.2, "b": 0.2, "a": 1.0},
                    "boundVariables": {"color": {"type": "VARIABLE_ALIAS", "id": var_id}},
                }]);
            }
            page_children.push(frame);
        }

        pages.push(json!({
            "id": page_id,
            "name": format!("Page {page_num}"),
            "type": "CANVAS",
            "absoluteBoundingBox": grid_bounds(&mut pos),
            "children": page_children,
        }));
    }

    json!({
        "name": format!("Bench Corpus ({nodes} nodes)"),
        "version": "1",
        "lastModified": "2026-01-01T00:00:00Z",
        "document": {
            "id": "0:0",
            "name": "Document",
            "type": "DOCUMENT",
            "absoluteBoundingBox": doc_bounds,
            "children": pages,
        },
        "components": components,
        "componentSets": component_sets,
        "styles": styles,
    })
}

// ---- derived query mix (shared by synthetic and real-file mode) ----

/// The load-test's tool-call parameters, always *derived* from the
/// flattened records rather than hardcoded — the same derivation runs in
/// synthetic mode (the corpus's own generated names/text) and real-file
/// mode (whatever's actually in the file), so bench exercises one code
/// path regardless of source. `Clone` so [`BenchSession`] can own its own
/// copy while [`PreparedBench`] keeps the original for the API comparison
/// phase.
#[derive(Clone)]
pub(crate) struct QueryPool {
    /// Distinct words drawn from node names and TEXT `characters`, sorted
    /// (a `BTreeSet` collection — deterministic, never a `HashMap`).
    words: Vec<String>,
    /// Every node id in the file, in flatten order.
    node_ids: Vec<String>,
    /// A real component or component-set name, if the file has one —
    /// `figmog_instances`'s target. `None` means the file has no
    /// components; that tool is skipped from the load mix.
    instances_target: Option<String>,
}

fn derive_query_pool(flattened: &Flattened) -> QueryPool {
    let mut words_set: BTreeSet<String> = BTreeSet::new();
    let mut node_ids: Vec<String> = Vec::new();
    // Prefer a component *set* name (groups variants — a more useful
    // `figmog_instances` target) over a standalone component's; flatten
    // order lists individual `components` map entries before
    // `componentSets` ones, so picking "whichever comes first" would
    // otherwise favor a single variant's name over the set's.
    let mut set_name: Option<String> = None;
    let mut standalone_component_name: Option<String> = None;

    for (id, rec) in &flattened.recs {
        match (id, rec) {
            (Id::Node(nid), Rec::Node(n)) => {
                node_ids.push(nid.clone());
                for w in n.name.split_whitespace() {
                    words_set.insert(w.to_string());
                }
                if let Some(t) = &n.text {
                    for w in t.split_whitespace() {
                        words_set.insert(w.to_string());
                    }
                }
            }
            (Id::ComponentSet(_), Rec::ComponentSet(cs)) if set_name.is_none() => {
                set_name = Some(cs.name.clone());
            }
            (Id::Component(_), Rec::Component(c)) if standalone_component_name.is_none() => {
                standalone_component_name = Some(c.name.clone());
            }
            _ => {}
        }
    }

    QueryPool {
        words: words_set.into_iter().collect(),
        node_ids,
        instances_target: set_name.or(standalone_component_name),
    }
}

// ---- percentiles ----

/// pN of a **sorted** slice: `v[(n - 1) * N / 100]` (integer floor
/// division). Empty input reports 0ms across the board.
fn percentile_ms(sorted: &[Duration], p: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let idx = (n - 1) * p / 100;
    sorted[idx].as_secs_f64() * 1000.0
}

fn max_ms(sorted: &[Duration]) -> f64 {
    sorted
        .last()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// ---- report types ----

#[derive(Debug, Serialize)]
pub struct CorpusStats {
    pub nodes: usize,
    pub bytes: usize,
    /// Synthetic mode: generation time. Real-file mode: the one Tier-1
    /// `GET /v1/files/:key` fetch time.
    pub gen_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct ColdStats {
    pub flatten_ms: f64,
    pub sync_ms: f64,
    pub records: usize,
    pub records_per_s: f64,
}

#[derive(Debug, Serialize)]
pub struct RepullStats {
    pub ms: f64,
    pub churn_zero: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolStats {
    pub tool: String,
    pub calls: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct LoadStats {
    pub per_tool: Vec<ToolStats>,
    pub total_calls: usize,
    pub wall_s: f64,
    pub req_per_s: f64,
}

/// Real-file-mode API comparison phase (spec §13): K sequential
/// `GET /nodes` calls timed the same way the serve load's `figmog_node`
/// calls are, plus figmog's own p50 for the side-by-side and the budget
/// math. `figmog_node_p50_ms`/`speedup_factor` are `None` when zero calls
/// succeeded (e.g. an immediate 429) — nothing to compare against.
#[derive(Debug, Serialize)]
pub struct ApiStats {
    pub calls: usize,
    pub p50_ms: f64,
    pub max_ms: f64,
    pub rate_limited: bool,
    pub retry_after_s: Option<u64>,
    pub figmog_node_p50_ms: Option<f64>,
    pub speedup_factor: Option<f64>,
    /// How long the serve load's `--calls` would take at Figma's ~10
    /// Tier-1 requests/minute budget, in minutes.
    pub budget_minutes_at_tier1_limit: f64,
}

#[derive(Debug, Serialize)]
pub struct BenchReport {
    /// `"synthetic"` or `"real"`.
    pub source: String,
    pub corpus: CorpusStats,
    pub cold: ColdStats,
    pub repull: RepullStats,
    pub load: LoadStats,
    /// `Some` only in real-file mode when the API comparison phase ran
    /// (i.e. not `--skip-api`).
    pub api: Option<ApiStats>,
}

/// `figmog bench` options. `exe` is the binary to spawn for the serve-load
/// phase: `figmog bench` itself passes `std::env::current_exe()` (correct
/// when a user runs `figmog bench` directly); tests pass the real compiled
/// binary via `assert_cmd::cargo::cargo_bin` so the path is right in both
/// contexts — `run` never tries to resolve it itself.
pub struct BenchOpts {
    pub nodes: usize,
    pub calls: usize,
    pub keep: bool,
    pub exe: PathBuf,
    /// Real Figma file key (already resolved from a bare key or URL — see
    /// `ident::parse_file_ref`). `None` selects synthetic mode.
    pub file: Option<String>,
    /// Real-file mode only: number of `GET /nodes` comparison calls.
    pub api_calls: usize,
    /// Real-file mode only: skip the API comparison phase entirely.
    pub skip_api: bool,
}

// ---- setup shared by one-shot `run` and interactive `run_interactive` ----

/// Phases 1-3 (corpus → cold sync → no-churn re-pull), assembled once and
/// reused by both entry points: [`run`]'s automated phase 4/5, and
/// [`run_interactive`]'s REPL. Owns the temp store's cleanup ([`TempDirGuard`])
/// so it survives exactly as long as whichever caller holds this struct.
struct PreparedBench {
    source: &'static str,
    corpus: CorpusStats,
    cold: ColdStats,
    repull: RepullStats,
    pool: QueryPool,
    db_path: PathBuf,
    exe: PathBuf,
    /// Real-file mode only (see [`BenchOpts::file`]).
    file_key: Option<String>,
    /// Real-file mode only: the same authenticated client phase 1 used to
    /// fetch the file, reused for the API comparison phase (one-shot) or
    /// the REPL's `api …` commands (interactive) instead of re-reading
    /// `FIGMA_TOKEN`.
    api_for_comparison: Option<UreqApi>,
    tmp_dir: PathBuf,
    keep: bool,
    #[allow(dead_code)] // held only for its Drop
    cleanup: TempDirGuard,
}

/// Phases 1-3 of [`run`]/[`run_interactive`]: corpus (synthetic generation
/// or one real-file Tier-1 fetch), cold sync into a fresh temp store, and a
/// no-churn re-pull of the identical in-memory data (the engine's headline
/// invariant, asserted in code). Returns `Err` if any phase fails or the
/// re-pull churns.
fn prepare(opts: &BenchOpts) -> Result<PreparedBench, String> {
    // ---- phase 1: corpus ----
    // `vars_resp` carries the opportunistic Enterprise `variables_local`
    // response (real-file mode only, like `do_pull` — spec §12); `Ok(None)`
    // on non-Enterprise plans is not an error.
    let (resp, vars_resp, api_for_comparison, source, gen_ms): (
        Value,
        Option<Value>,
        Option<UreqApi>,
        &str,
        f64,
    ) = match &opts.file {
        Some(key) => {
            let token = std::env::var("FIGMA_TOKEN").map_err(|_| {
                "FIGMA_TOKEN not set — required for `figmog bench <file>`".to_string()
            })?;
            let api = UreqApi::new(token);
            let fetch_start = Instant::now();
            let resp = api.file(key).map_err(|e| e.to_string())?;
            let gen_ms = fetch_start.elapsed().as_secs_f64() * 1000.0;
            let vars_resp = api.variables_local(key).map_err(|e| e.to_string())?;
            (resp, vars_resp, Some(api), "real", gen_ms)
        }
        None => {
            let gen_start = Instant::now();
            let resp = generate_corpus(opts.nodes);
            let gen_ms = gen_start.elapsed().as_secs_f64() * 1000.0;
            (resp, None, None, "synthetic", gen_ms)
        }
    };

    // ---- phase 2: cold sync ----
    let flatten_start = Instant::now();
    let mut flattened = flatten_file(&resp).map_err(|e| e.to_string())?;
    if let Some(v) = &vars_resp {
        let var_recs = crate::vars::parse_variables_export(v).map_err(|e| e.to_string())?;
        flattened.recs.extend(var_recs);
    }
    let flatten_ms = flatten_start.elapsed().as_secs_f64() * 1000.0;

    let node_count = flattened
        .recs
        .iter()
        .filter(|(id, _)| matches!(id, Id::Node(_)))
        .count();
    let bytes = serde_json::to_vec(&resp).map_err(|e| e.to_string())?.len();

    let tmp_dir = make_temp_dir()?;
    let db_path = tmp_dir.join("db");
    let cleanup = TempDirGuard {
        path: tmp_dir.clone(),
        keep: opts.keep,
    };

    let mut st = open_store_checked(|| crate::open_store!(&db_path))?;

    let sync_start = Instant::now();
    let churn = store::sync(&mut st, &BTreeSet::new(), &flattened, 0);
    let sync_ms = sync_start.elapsed().as_secs_f64() * 1000.0;
    let records = flattened.recs.len();
    let records_per_s = if sync_ms > 0.0 {
        records as f64 / (sync_ms / 1000.0)
    } else {
        0.0
    };
    debug_assert!(churn.removed == 0, "fresh store: nothing to remove");

    // ---- phase 3: no-churn re-pull (same in-memory data, no re-fetch) ----
    let prior = st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
        collect_sweepable(&nodes, &components, &component_sets, &styles)
    });
    let repull_start = Instant::now();
    let churn2 = store::sync(&mut st, &prior, &flattened, 0);
    let repull_ms = repull_start.elapsed().as_secs_f64() * 1000.0;
    if churn2.added != 0 || churn2.changed != 0 || churn2.removed != 0 {
        return Err(format!(
            "bench invariant violated: non-zero churn on re-pull of identical data: {churn2:?}"
        ));
    }

    // Store must be closed before the serve child opens the same path —
    // fjall allows only one writer.
    drop(st);

    // ---- derived query mix (one code path for both modes) ----
    let pool = derive_query_pool(&flattened);

    Ok(PreparedBench {
        source,
        corpus: CorpusStats {
            nodes: node_count,
            bytes,
            gen_ms,
        },
        cold: ColdStats {
            flatten_ms,
            sync_ms,
            records,
            records_per_s,
        },
        repull: RepullStats {
            ms: repull_ms,
            churn_zero: true,
        },
        pool,
        db_path,
        exe: opts.exe.clone(),
        file_key: opts.file.clone(),
        api_for_comparison,
        tmp_dir,
        keep: opts.keep,
        cleanup,
    })
}

/// Owns the spawned `figmog serve` child, its stdio pump, the derived query
/// pool, and cumulative per-call stats — the shared primitive behind both
/// the one-shot load phase ([`run_load_phase`]) and the interactive REPL
/// ([`crate::repl::run`]). [`BenchSession::fire`] is the one thing both
/// drive a `tools/call` frame over the child's stdio pipe with.
pub(crate) struct BenchSession {
    guard: ChildGuard,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    pool: QueryPool,
    rotation: Vec<&'static str>,
    rng: Lcg,
    next_id: i64,
    mix_counter: usize,
    stats: Vec<(String, Duration)>,
}

impl BenchSession {
    /// Spawn `figmog serve --no-upstream --no-watch --db <db>` and complete
    /// the MCP handshake. `Err` if the pool has nothing to query at all
    /// (spec §13: "nothing to load-test") or the handshake fails.
    pub(crate) fn start(
        exe: &std::path::Path,
        db_path: &std::path::Path,
        pool: QueryPool,
    ) -> Result<Self, String> {
        let rotation = tool_rotation(&pool);
        if rotation.is_empty() {
            return Err("bench corpus has neither searchable words, nodes, nor components — nothing to load-test".into());
        }
        let (guard, mut stdin, rx) = spawn_serve(exe, db_path);

        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
            }),
        )?;
        recv(&rx)?; // initialize response; contents not needed here
        send(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )?;

        Ok(BenchSession {
            guard,
            stdin: Some(stdin),
            rx,
            pool,
            rotation,
            rng: Lcg::new(LCG_SEED),
            next_id: 2,
            mix_counter: 0,
            stats: Vec::new(),
        })
    }

    /// Fire one raw `tools/call` frame, timed write→response-line, and
    /// record it into the session's cumulative stats. `Err` only on a
    /// transport failure (send/recv/parse) — a tool result with
    /// `isError: true` is still `Ok`, so callers (the one-shot load loop,
    /// the REPL) each decide how to react to it.
    pub(crate) fn fire(&mut self, tool: &str, args: Value) -> Result<(Duration, Value), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "bench session already finished".to_string())?;
        let req_id = self.next_id;
        self.next_id += 1;

        let start = Instant::now();
        send(
            stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {"name": tool, "arguments": args},
            }),
        )?;
        let resp = recv(&self.rx)?;
        let elapsed = start.elapsed();
        self.stats.push((tool.to_string(), elapsed));
        Ok((elapsed, resp))
    }

    /// Next `(tool, args)` pair in the fixed rotating mix (spec §13),
    /// derived from this session's query pool — continues the same
    /// rotation across however many calls this session has already fired.
    pub(crate) fn next_mixed_call(&mut self) -> (&'static str, Value) {
        let tool = self.rotation[self.mix_counter % self.rotation.len()];
        self.mix_counter += 1;
        let args = call_args(tool, &self.pool, &mut self.rng);
        (tool, args)
    }

    /// Every `(tool, elapsed)` fired this session, in fire order.
    pub(crate) fn stats(&self) -> &[(String, Duration)] {
        &self.stats
    }

    /// Close stdin (EOF — how `--no-watch` serve exits cleanly) and wait
    /// for the child to exit. Idempotent: a second call just re-waits an
    /// already-exited child. `ChildGuard`'s `Drop` is the safety net if
    /// this is never reached (an early `?` return, a panic) — it
    /// kills+waits unconditionally, so the child is never left a zombie.
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        self.stdin.take(); // dropped here -> EOF on the child's stdin
        wait_with_timeout(&mut self.guard.0, EXIT_TIMEOUT)
    }
}

// ---- run ----

/// Run every phase and return the assembled report, or `Err` if any phase
/// fails or any tool call comes back `isError` (a graceful 429 in the API
/// comparison phase is a *recorded* result, not a failure — see
/// [`ApiStats`]).
pub fn run(opts: BenchOpts) -> Result<BenchReport, String> {
    let prepared = prepare(&opts)?;

    // ---- phase 4: serve load ----
    let load = run_load_phase(&opts, &prepared.exe, &prepared.db_path, &prepared.pool)?;

    // ---- phase 5: API comparison (real-file mode only) ----
    let api = match (&prepared.file_key, &prepared.api_for_comparison) {
        (Some(key), Some(api)) if !opts.skip_api => Some(run_api_comparison_phase(
            api,
            key,
            opts.api_calls,
            &prepared.pool,
            &load,
        )?),
        _ => None,
    };

    let report = BenchReport {
        source: prepared.source.to_string(),
        corpus: prepared.corpus,
        cold: prepared.cold,
        repull: prepared.repull,
        load,
        api,
    };

    if prepared.keep {
        eprintln!("figmog: kept temp store at {}", prepared.tmp_dir.display());
    }
    // `prepared` (and its `TempDirGuard`) drops here, cleaning up the temp
    // store unless `--keep`.

    Ok(report)
}

/// `figmog bench --interactive` (build design §13 "Interactive mode"): the
/// same setup as [`run`] (corpus/real file → cold sync → no-churn re-pull →
/// serve child spawn), then a REPL on the terminal instead of the
/// automated load/API phases — requests visible as they fire.
pub fn run_interactive(opts: BenchOpts) -> Result<(), String> {
    let mut prepared = prepare(&opts)?;
    print_setup_human(
        prepared.source,
        &prepared.corpus,
        &prepared.cold,
        &prepared.repull,
    );
    println!();

    let real_file = match (prepared.file_key.take(), prepared.api_for_comparison.take()) {
        (Some(key), Some(api)) => Some(crate::repl::RealFileCtx { key, api }),
        _ => None,
    };

    let mut session = BenchSession::start(&prepared.exe, &prepared.db_path, prepared.pool.clone())?;
    let repl_result = crate::repl::run(&mut session, real_file);
    let finish_result = session.finish();

    if prepared.keep {
        eprintln!("figmog: kept temp store at {}", prepared.tmp_dir.display());
    }

    repl_result?;
    finish_result
}

// ---- temp dir management ----

struct TempDirGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A fresh, empty temp directory for this bench run. Not derived from
/// `tempfile` (a dev-only dependency elsewhere in this crate) — a
/// process-id-qualified path under the system temp dir is unique enough
/// for one bench run per process, and this isn't part of any timed or
/// deterministic path (spec §13's "no SystemTime in the measurement
/// path" is about the timed phases, not directory naming).
fn make_temp_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("figmog-bench-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("clearing stale temp dir: {e}"))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating temp dir: {e}"))?;
    Ok(dir)
}

// ---- serve load phase ----

/// Kills the spawned `figmog serve` child on drop, so a failed assertion
/// or early `?` return never leaves an orphaned process behind (mirrors
/// `tests/serve.rs`'s `ChildGuard`).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

fn spawn_serve(
    exe: &std::path::Path,
    db: &std::path::Path,
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let mut child = Command::new(exe)
        .args(["serve", "--no-upstream", "--no-watch", "--db"])
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn figmog serve for bench");

    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");

    // Drain stderr purely so the child never blocks on a full pipe; never
    // asserted on (it's just serve's own startup log line).
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[figmog serve] {line}");
        }
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    (ChildGuard(child), stdin, rx)
}

fn send(stdin: &mut ChildStdin, msg: &Value) -> Result<(), String> {
    writeln!(stdin, "{msg}").map_err(|e| format!("writing to serve child: {e}"))?;
    stdin
        .flush()
        .map_err(|e| format!("flushing serve child stdin: {e}"))
}

fn recv(rx: &Receiver<String>) -> Result<Value, String> {
    let line = rx
        .recv_timeout(CALL_TIMEOUT)
        .map_err(|_| "figmog serve did not respond within the timeout".to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("response line was not valid JSON: {e}"))
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("figmog serve exited with {status:?}"))
            };
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err(format!(
                "figmog serve did not exit within {timeout:?} of stdin EOF"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The fixed rotating tool-call mix (spec §13), narrowed to what the file
/// actually supports: `figmog_search`/`figmog_instances` are dropped (with
/// a stderr note) when the corpus has no searchable words / no components.
fn tool_rotation(pool: &QueryPool) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if pool.words.is_empty() {
        eprintln!(
            "figmog: bench corpus has no searchable words — dropping figmog_search from the load mix"
        );
    } else {
        kinds.push("figmog_search");
    }
    if pool.node_ids.is_empty() {
        eprintln!("figmog: bench corpus has no nodes — dropping figmog_node from the load mix");
    } else {
        kinds.push("figmog_node");
    }
    kinds.push("figmog_where");
    kinds.push("figmog_stats");
    kinds.push("figmog_tree");
    if pool.instances_target.is_some() {
        kinds.push("figmog_instances");
    } else {
        eprintln!(
            "figmog: bench corpus has no components — dropping figmog_instances from the load mix"
        );
    }
    kinds
}

fn call_args(tool: &str, pool: &QueryPool, rng: &mut Lcg) -> Value {
    match tool {
        "figmog_search" => {
            let word = &pool.words[rng.next_range(pool.words.len())];
            json!({"query": word})
        }
        "figmog_node" => {
            let id = &pool.node_ids[rng.next_range(pool.node_ids.len())];
            json!({"id": id})
        }
        "figmog_where" => json!({"pointer": "/layoutMode", "equals": "VERTICAL"}),
        "figmog_stats" => json!({}),
        "figmog_tree" => json!({"depth": 2}),
        "figmog_instances" => {
            json!({"target": pool.instances_target.as_deref().unwrap_or_default()})
        }
        other => unreachable!("tool_rotation only emits known tool names, got {other}"),
    }
}

fn run_load_phase(
    opts: &BenchOpts,
    exe: &std::path::Path,
    db_path: &std::path::Path,
    pool: &QueryPool,
) -> Result<LoadStats, String> {
    let mut session = BenchSession::start(exe, db_path, pool.clone())?;
    let rotation = session.rotation.clone();
    let rotation_len = rotation.len();
    let mut timings: Vec<Vec<Duration>> = vec![Vec::new(); rotation_len];

    let wall_start = Instant::now();
    for i in 0..opts.calls {
        let tool_idx = i % rotation_len;
        let (tool, args) = session.next_mixed_call();
        let (elapsed, resp) = session.fire(tool, args)?;

        if resp["result"]["isError"] == json!(true) {
            let text = resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("<no message>");
            return Err(format!(
                "bench load call #{i} ({tool}) returned isError: {text}"
            ));
        }
        timings[tool_idx].push(elapsed);
    }
    let wall = wall_start.elapsed();

    session.finish()?;

    let mut per_tool = Vec::with_capacity(rotation_len);
    for (tool, mut durations) in rotation.into_iter().zip(timings) {
        durations.sort();
        per_tool.push(ToolStats {
            tool: tool.to_string(),
            calls: durations.len(),
            p50_ms: percentile_ms(&durations, 50),
            p95_ms: percentile_ms(&durations, 95),
            p99_ms: percentile_ms(&durations, 99),
            max_ms: max_ms(&durations),
        });
    }

    let wall_s = wall.as_secs_f64();
    let req_per_s = if wall_s > 0.0 {
        opts.calls as f64 / wall_s
    } else {
        0.0
    };

    Ok(LoadStats {
        per_tool,
        total_calls: opts.calls,
        wall_s,
        req_per_s,
    })
}

/// Group timed calls by tool, preserving each tool's first-appearance
/// order (never a `HashMap` — spec's no-HashMap-iteration-order-at-an-
/// output-boundary rule applies here too). Used by the REPL's `run N`
/// burst table and cumulative `report` table, where (unlike the one-shot
/// load phase's fixed rotation) the set of tools fired isn't known ahead
/// of time.
pub(crate) fn group_tool_stats(entries: &[(String, Duration)]) -> Vec<ToolStats> {
    let mut grouped: Vec<(String, Vec<Duration>)> = Vec::new();
    for (tool, d) in entries {
        match grouped.iter_mut().find(|(t, _)| t == tool) {
            Some((_, durations)) => durations.push(*d),
            None => grouped.push((tool.clone(), vec![*d])),
        }
    }
    grouped
        .into_iter()
        .map(|(tool, mut durations)| {
            durations.sort();
            ToolStats {
                tool,
                calls: durations.len(),
                p50_ms: percentile_ms(&durations, 50),
                p95_ms: percentile_ms(&durations, 95),
                p99_ms: percentile_ms(&durations, 99),
                max_ms: max_ms(&durations),
            }
        })
        .collect()
}

// ---- API comparison phase (real-file mode) ----

fn run_api_comparison_phase(
    api: &UreqApi,
    key: &str,
    api_calls: usize,
    pool: &QueryPool,
    load: &LoadStats,
) -> Result<ApiStats, String> {
    // Continue the same deterministic stream the load phase's arg-picking
    // used, rather than reusing its exact seed — the two phases just need
    // *a* reproducible id sequence each, not a shared one.
    let mut rng = Lcg::new(LCG_SEED.wrapping_add(1));
    let mut durations: Vec<Duration> = Vec::new();
    let mut rate_limited = false;
    let mut retry_after_s: Option<u64> = None;

    let api_calls = if pool.node_ids.is_empty() {
        eprintln!("figmog: bench corpus has no nodes — skipping the API comparison phase");
        0
    } else {
        api_calls
    };
    for _ in 0..api_calls {
        let id = &pool.node_ids[rng.next_range(pool.node_ids.len())];
        let start = Instant::now();
        match api.file_nodes(key, id) {
            Ok(_) => durations.push(start.elapsed()),
            Err(ApiError::RateLimited { retry_after }) => {
                rate_limited = true;
                retry_after_s = Some(retry_after.as_secs());
                eprintln!(
                    "figmog: bench API comparison phase rate-limited (429) after {} call(s); retry after {}s — ending the phase gracefully",
                    durations.len(),
                    retry_after.as_secs()
                );
                break;
            }
            Err(e) => return Err(format!("API comparison phase failed: {e}")),
        }
    }

    if !rate_limited {
        // One Tier-3 meta call for reference (spec §13); skipped after a
        // 429 to avoid spending more of the just-exhausted budget.
        if let Err(e) = api.file_meta(key) {
            eprintln!("figmog: bench API comparison reference meta call failed (non-fatal): {e}");
        }
    }

    // `run_api_comparison_phase` only ever runs in real-file mode, which
    // always attempts exactly one `file()` fetch and one `variables_local`
    // call up front (see `run`'s corpus phase) — both already spent by the
    // time this phase starts.
    eprintln!(
        "figmog: API calls spent — file:1 variables:1 nodes:{} meta:{}",
        durations.len(),
        if rate_limited { 0 } else { 1 }
    );

    durations.sort();
    let calls = durations.len();
    let p50 = percentile_ms(&durations, 50);
    let max = max_ms(&durations);

    let figmog_node_p50_ms = load
        .per_tool
        .iter()
        .find(|t| t.tool == "figmog_node")
        .map(|t| t.p50_ms);
    let speedup_factor = match (figmog_node_p50_ms, calls > 0) {
        (Some(fig), true) if fig > 0.0 => Some(p50 / fig),
        _ => None,
    };

    Ok(ApiStats {
        calls,
        p50_ms: p50,
        max_ms: max,
        rate_limited,
        retry_after_s,
        figmog_node_p50_ms,
        speedup_factor,
        budget_minutes_at_tier1_limit: load.total_calls as f64 / 10.0,
    })
}

// ---- human-readable report ----

/// The corpus/cold-sync/re-pull setup lines, shared by [`print_human`]
/// (the one-shot report) and [`run_interactive`] (printed once before the
/// REPL takes over).
fn print_setup_human(source: &str, corpus: &CorpusStats, cold: &ColdStats, repull: &RepullStats) {
    println!(
        "corpus  [{source}]  {} nodes, {} bytes, {:.1}ms",
        corpus.nodes, corpus.bytes, corpus.gen_ms
    );
    println!(
        "cold sync    {:.1}ms flatten + {:.1}ms sync, {} records ({:.0} records/s)",
        cold.flatten_ms, cold.sync_ms, cold.records, cold.records_per_s
    );
    println!(
        "re-pull      {:.1}ms, churn zero: {}",
        repull.ms, repull.churn_zero
    );
}

/// The per-tool percentile table, shared by [`print_human`] (the one-shot
/// load phase's fixed rotation) and the REPL's `run N`/`report` commands
/// (an arbitrary set of tools, grouped by [`group_tool_stats`]).
pub(crate) fn print_tool_table(per_tool: &[ToolStats]) {
    println!(
        "{:<18} {:>8} {:>10} {:>10} {:>10} {:>10}",
        "tool", "calls", "p50 (ms)", "p95 (ms)", "p99 (ms)", "max (ms)"
    );
    for t in per_tool {
        println!(
            "{:<18} {:>8} {:>10.3} {:>10.3} {:>10.3} {:>10.3}",
            t.tool, t.calls, t.p50_ms, t.p95_ms, t.p99_ms, t.max_ms
        );
    }
}

/// The phase lines + per-tool table + headline (`--json`'s alternative;
/// stdout purity means callers pick exactly one).
pub fn print_human(report: &BenchReport) {
    print_setup_human(&report.source, &report.corpus, &report.cold, &report.repull);
    println!();
    print_tool_table(&report.load.per_tool);
    println!();
    println!(
        "figmog served {} queries in {:.1}s ({:.0} req/s). Figma's Tier-1 API budget on a free plan: ~10 file requests per MINUTE.",
        report.load.total_calls, report.load.wall_s, report.load.req_per_s
    );

    if let Some(api) = &report.api {
        println!();
        println!(
            "API comparison  {} call(s), p50 {:.1}ms, max {:.1}ms{}",
            api.calls,
            api.p50_ms,
            api.max_ms,
            if api.rate_limited {
                format!(
                    " — rate-limited (429), retry after {}s",
                    api.retry_after_s.unwrap_or_default()
                )
            } else {
                String::new()
            }
        );
        match (api.figmog_node_p50_ms, api.speedup_factor) {
            (Some(fig), Some(speedup)) => {
                println!(
                    "figmog_node p50 {:.3}ms vs API /nodes p50 {:.1}ms — figmog is {:.0}x faster",
                    fig, api.p50_ms, speedup
                );
            }
            (Some(fig), None) => {
                println!("figmog_node p50 {fig:.3}ms; no successful API call to compare against");
            }
            _ => {}
        }
        println!(
            "at Figma's ~10 Tier-1 requests/minute, {} calls would take ≈{:.1} minutes — figmog's serve load: {:.1}s",
            report.load.total_calls, api.budget_minutes_at_tier1_limit, report.load.wall_s
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- corpus generation ----

    fn node_ids(v: &Value) -> Vec<String> {
        let flattened = flatten_file(v).expect("bench corpus flattens");
        flattened
            .recs
            .iter()
            .filter_map(|(id, _)| match id {
                Id::Node(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn exact_node_count_100() {
        assert_eq!(node_ids(&generate_corpus(100)).len(), 100);
    }

    #[test]
    fn exact_node_count_1000() {
        assert_eq!(node_ids(&generate_corpus(1000)).len(), 1000);
    }

    #[test]
    fn exact_node_count_10007_odd() {
        assert_eq!(node_ids(&generate_corpus(10007)).len(), 10007);
    }

    #[test]
    fn deterministic_byte_identical_across_calls() {
        let a = serde_json::to_vec(&generate_corpus(500)).unwrap();
        let b = serde_json::to_vec(&generate_corpus(500)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn contains_component_set_instance_and_nonempty_text() {
        let corpus = generate_corpus(500);
        let flattened = flatten_file(&corpus).unwrap();

        let has_component_set = flattened
            .recs
            .iter()
            .any(|(id, _)| matches!(id, Id::ComponentSet(_)));
        assert!(has_component_set, "expected at least one COMPONENT_SET");

        let mut has_instance = false;
        let mut text_nodes_nonempty = true;
        let mut saw_text_node = false;
        for (_, rec) in &flattened.recs {
            if let Rec::Node(n) = rec {
                if n.node_type == "INSTANCE" {
                    has_instance = true;
                }
                if n.node_type == "TEXT" {
                    saw_text_node = true;
                    if n.text.as_deref().unwrap_or("").is_empty() {
                        text_nodes_nonempty = false;
                    }
                }
            }
        }
        assert!(has_instance, "expected at least one INSTANCE");
        assert!(saw_text_node, "expected at least one TEXT node");
        assert!(
            text_nodes_nonempty,
            "every TEXT node should have non-empty characters"
        );
    }

    #[test]
    fn flatten_file_succeeds_and_yields_exact_node_records() {
        for n in [100usize, 1000, 10007] {
            let corpus = generate_corpus(n);
            let flattened = flatten_file(&corpus).expect("bench corpus flattens");
            let count = flattened
                .recs
                .iter()
                .filter(|(id, _)| matches!(id, Id::Node(_)))
                .count();
            assert_eq!(count, n, "node count mismatch for nodes={n}");
        }
    }

    // ---- percentiles ----

    #[test]
    fn percentile_math_on_a_known_vector() {
        let sorted: Vec<Duration> = (1..=10).map(Duration::from_millis).collect();
        // pN = v[(n-1)*N/100], n=10: idx = 9*N/100 (integer floor).
        assert_eq!(percentile_ms(&sorted, 50), 5.0); // idx (9*50)/100=4 -> v[4]=5ms
        assert_eq!(percentile_ms(&sorted, 95), 9.0); // idx (9*95)/100=8 -> v[8]=9ms
        assert_eq!(percentile_ms(&sorted, 99), 9.0); // idx (9*99)/100=8 -> v[8]=9ms
        assert_eq!(max_ms(&sorted), 10.0);
    }

    #[test]
    fn percentile_of_empty_is_zero() {
        let empty: Vec<Duration> = Vec::new();
        assert_eq!(percentile_ms(&empty, 50), 0.0);
        assert_eq!(max_ms(&empty), 0.0);
    }

    #[test]
    fn percentile_of_single_value() {
        let one = vec![Duration::from_millis(7)];
        assert_eq!(percentile_ms(&one, 50), 7.0);
        assert_eq!(percentile_ms(&one, 99), 7.0);
        assert_eq!(max_ms(&one), 7.0);
    }

    // ---- derived query pool ----

    #[test]
    fn query_pool_derives_from_flattened_records_not_hardcoded() {
        let corpus = generate_corpus(500);
        let flattened = flatten_file(&corpus).unwrap();
        let pool = derive_query_pool(&flattened);
        assert!(!pool.words.is_empty());
        assert!(!pool.node_ids.is_empty());
        assert_eq!(pool.instances_target.as_deref(), Some("Button"));
    }

    #[test]
    fn query_pool_handles_a_file_with_no_components() {
        let file = json!({
            "name": "F", "version": "1", "lastModified": "t",
            "document": {
                "id": "0:0", "name": "Document", "type": "DOCUMENT",
                "children": [
                    {"id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                        {"id": "1:1", "name": "Hello World", "type": "TEXT", "characters": "Hello World", "children": []}
                    ]}
                ]
            },
            "components": {}, "componentSets": {}, "styles": {},
        });
        let flattened = flatten_file(&file).unwrap();
        let pool = derive_query_pool(&flattened);
        assert!(pool.instances_target.is_none());
        assert!(pool.words.contains(&"Hello".to_string()));

        let rotation = tool_rotation(&pool);
        assert!(!rotation.contains(&"figmog_instances"));
        assert!(rotation.contains(&"figmog_search"));
    }
}
