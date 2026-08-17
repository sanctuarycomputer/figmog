//! `figmog bench --interactive` — the live REPL (build design §13
//! "Interactive mode"). Requests visible as they fire: one aligned line per
//! tool call (sequence number, tool, arg digest, latency, result digest),
//! a live mixed-workload burst (`run N`), cumulative session percentiles
//! (`report`), and (real-file mode only) live Figma API comparison calls
//! (`api node <id>` / `api meta`).
//!
//! Colors are raw ANSI escapes, emitted only when stdout is a terminal
//! ([`std::io::IsTerminal`]) — piped/non-TTY stdout is always plain text,
//! zero `\x1b` bytes (the scripted e2e in `tests/cli.rs` asserts this).
//! No readline: plain `stdin().read_line` is enough for a demo REPL (spec
//! §13's non-goals).

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::bench::{BenchSession, group_tool_stats, print_tool_table};
use crate::cli::parse_equals;

/// Enables the `api node <id>` / `api meta` commands — real-file mode
/// only. `key` is the Figma file key `figmog bench` resolved at startup;
/// `api` is the same authenticated client phase 1 used for the initial
/// fetch (no extra `FIGMA_TOKEN` read).
pub(crate) struct RealFileCtx {
    pub(crate) key: String,
    pub(crate) api: UreqApi,
}

/// One parsed REPL input line. Every tool shorthand (`search`, `node`,
/// `tree`, …) parses straight to `Tool { name, args }` — a ready-to-fire
/// `figmog_*` tool name plus the JSON arguments the shorthand builds, so
/// the dispatch loop doesn't need to know the shorthand grammar at all.
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Help,
    Quit,
    Run(usize),
    Report,
    Api(ApiCmd),
    /// The raw `call <tool> <json-args>` escape hatch — a caller-supplied
    /// tool name (works for proxied tools too, when an upstream is
    /// attached) and already-parsed JSON arguments.
    Call {
        tool: String,
        args: Value,
    },
    /// A tool shorthand, already resolved to a `figmog_*` tool name and
    /// its JSON arguments.
    Tool {
        name: String,
        args: Value,
    },
}

#[derive(Debug, PartialEq)]
pub(crate) enum ApiCmd {
    Node(String),
    Meta,
}

fn tool_cmd(name: &str, args: Value) -> Command {
    Command::Tool {
        name: name.to_string(),
        args,
    }
}

/// Parse one REPL input line into a [`Command`]. `Err` carries a
/// human-readable message the REPL prints as-is (never a panic on bad
/// input — this is the only thing standing between a typo and a crashed
/// demo).
pub(crate) fn parse_line(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let mut tokens = line.split_whitespace();
    let cmd = tokens.next().ok_or_else(|| "empty command".to_string())?;
    let rest: Vec<&str> = tokens.collect();

    match cmd {
        "help" => Ok(Command::Help),
        "quit" => Ok(Command::Quit),
        "report" => Ok(Command::Report),

        "run" => {
            let n = rest
                .first()
                .ok_or_else(|| "run: missing count, e.g. `run 20`".to_string())?;
            let n: usize = n.parse().map_err(|_| format!("run: not a number: {n}"))?;
            Ok(Command::Run(n))
        }

        "api" => match rest.first() {
            Some(&"node") => {
                let id = rest
                    .get(1)
                    .ok_or_else(|| "api node: missing id".to_string())?;
                Ok(Command::Api(ApiCmd::Node((*id).to_string())))
            }
            Some(&"meta") => Ok(Command::Api(ApiCmd::Meta)),
            Some(other) => Err(format!("api: unknown subcommand: {other}")),
            None => Err("api: missing subcommand (node <id> | meta)".to_string()),
        },

        "call" => {
            // Reconstruct from the original (untokenized) line so JSON
            // arguments keep their exact spacing/quoting — `rest.join(" ")`
            // would collapse runs of whitespace inside string literals.
            let after_cmd = line["call".len()..].trim_start();
            let mut it = after_cmd.splitn(2, char::is_whitespace);
            let tool = it
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "call: missing tool name".to_string())?;
            let args_raw = it.next().unwrap_or("").trim();
            let args: Value = if args_raw.is_empty() {
                json!({})
            } else {
                serde_json::from_str(args_raw)
                    .map_err(|e| format!("call: invalid JSON args: {e}"))?
            };
            Ok(Command::Call {
                tool: tool.to_string(),
                args,
            })
        }

        // ---- tool shorthands (spec §13's list) ----
        "status" => Ok(tool_cmd("figmog_status", json!({}))),
        "pages" => Ok(tool_cmd("figmog_pages", json!({}))),
        "components" => Ok(tool_cmd("figmog_components", json!({}))),
        "stats" => Ok(tool_cmd("figmog_stats", json!({}))),

        "search" => {
            if rest.is_empty() {
                return Err("search: missing query words, e.g. `search button`".to_string());
            }
            Ok(tool_cmd("figmog_search", json!({"query": rest.join(" ")})))
        }

        "node" => {
            let id = rest.first().ok_or_else(|| "node: missing id".to_string())?;
            let mut args = json!({"id": id});
            if rest.get(1) == Some(&"children") {
                args["children"] = json!(true);
            }
            Ok(tool_cmd("figmog_node", args))
        }

        "tree" => {
            let mut args = json!({});
            if let Some(id) = rest.first() {
                args["id"] = json!(*id);
            }
            if let Some(depth) = rest.get(1) {
                let d: usize = depth
                    .parse()
                    .map_err(|_| format!("tree: depth not a number: {depth}"))?;
                args["depth"] = json!(d);
            }
            Ok(tool_cmd("figmog_tree", args))
        }

        "find" => {
            let ty = rest
                .first()
                .ok_or_else(|| "find: missing TYPE, e.g. `find FRAME`".to_string())?;
            let mut args = json!({"type": ty});
            if let Some(page) = rest.get(1) {
                args["page"] = json!(*page);
            }
            Ok(tool_cmd("figmog_find", args))
        }

        "where" => {
            let pointer = rest
                .first()
                .ok_or_else(|| "where: missing pointer, e.g. `where /layoutMode`".to_string())?;
            let mut args = json!({"pointer": pointer});
            if rest.len() > 1 {
                args["equals"] = parse_equals(&rest[1..].join(" "));
            }
            Ok(tool_cmd("figmog_where", args))
        }

        "path" => {
            let id = rest.first().ok_or_else(|| "path: missing id".to_string())?;
            Ok(tool_cmd("figmog_path", json!({"id": id})))
        }

        "text" => {
            let mut args = json!({});
            if let Some(page) = rest.first() {
                args["page"] = json!(*page);
            }
            Ok(tool_cmd("figmog_text", args))
        }

        "at" => {
            let x: f64 = rest
                .first()
                .ok_or_else(|| "at: missing x, e.g. `at 100 200`".to_string())?
                .parse()
                .map_err(|_| "at: x is not a number".to_string())?;
            let y: f64 = rest
                .get(1)
                .ok_or_else(|| "at: missing y, e.g. `at 100 200`".to_string())?
                .parse()
                .map_err(|_| "at: y is not a number".to_string())?;
            Ok(tool_cmd("figmog_at", json!({"x": x, "y": y})))
        }

        "instances" => {
            if rest.is_empty() {
                return Err("instances: missing target".to_string());
            }
            Ok(tool_cmd(
                "figmog_instances",
                json!({"target": rest.join(" ")}),
            ))
        }

        "styles" => {
            let mut args = json!({});
            if let Some(t) = rest.first() {
                args["type"] = json!(*t);
            }
            Ok(tool_cmd("figmog_styles", args))
        }

        "uses" => {
            let id = rest.first().ok_or_else(|| "uses: missing id".to_string())?;
            Ok(tool_cmd("figmog_uses", json!({"id": id})))
        }

        "vars" => {
            let mut args = json!({});
            if let Some(id) = rest.first() {
                args["id"] = json!(*id);
            }
            Ok(tool_cmd("figmog_vars", args))
        }

        other => Err(format!("unknown command: {other} (try `help`)")),
    }
}

// ---- color / formatting ----

enum Color {
    Green,
    Yellow,
    Red,
}

/// Raw ANSI escapes, only when `tty` — non-TTY stdout gets `s` back
/// unchanged (zero `\x1b` bytes, spec §13).
fn paint(s: &str, color: Color, tty: bool) -> String {
    if !tty {
        return s.to_string();
    }
    let code = match color {
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Red => "31",
    };
    format!("\x1b[{code}m{s}\x1b[0m")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Latency thresholds per spec §13: green under 10ms, yellow under 100ms,
/// red at/above — errors are always red regardless of how fast they came
/// back.
fn colorize_ms(ms: f64, tty: bool, is_error: bool) -> String {
    let s = format!("{ms:>8.2}ms");
    let color = if is_error {
        Color::Red
    } else if ms < 10.0 {
        Color::Green
    } else if ms < 100.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    paint(&s, color, tty)
}

/// Result digest per spec §13: hit count for array results, `name` for
/// node-shaped (or any named-object) results, the `isError` text for
/// errors. Anything else (an object with no `name`, e.g. `figmog_stats`)
/// falls back to `"ok"` — the digest's job is a live pulse, not a full
/// dump.
fn result_digest(resp: &Value) -> (String, bool) {
    let is_error = resp["result"]["isError"] == json!(true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    if is_error {
        return (text.to_string(), true);
    }
    let digest = match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(a)) => format!("{} hits", a.len()),
        Ok(Value::Object(o)) => o
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "ok".to_string()),
        _ => "ok".to_string(),
    };
    (digest, false)
}

/// `#{seq:>4}  {tool:<18} {args_digest:<32} {ms:>8.2}ms  {digest}` (spec
/// §13). Plain-mode (`tty: false`) output is asserted byte-for-byte in
/// unit tests; TTY mode only adds ANSI color around the latency/digest
/// tokens, so the column layout is identical either way.
pub(crate) fn format_latency_line(
    seq: usize,
    tool: &str,
    args: &Value,
    elapsed: Duration,
    resp: &Value,
    tty: bool,
) -> String {
    let ms = elapsed.as_secs_f64() * 1000.0;
    let args_digest = truncate(&compact_json(args), 32);
    let (digest, is_error) = result_digest(resp);
    let ms_str = colorize_ms(ms, tty, is_error);
    let digest_str = if is_error {
        paint(&digest, Color::Red, tty)
    } else {
        digest
    };
    format!("#{seq:>4}  {tool:<18} {args_digest:<32} {ms_str}  {digest_str}")
}

#[allow(clippy::too_many_arguments)]
fn print_api_line(
    seq: usize,
    label: &str,
    arg_digest: &str,
    ms: f64,
    is_error: bool,
    digest: &str,
    note: &str,
    tty: bool,
) {
    let ms_str = colorize_ms(ms, tty, is_error);
    let digest_str = if is_error {
        paint(digest, Color::Red, tty)
    } else {
        digest.to_string()
    };
    println!("#{seq:>4}  {label:<18} {arg_digest:<32} {ms_str}  {digest_str}  ({note})");
}

fn cmd_api_node(ctx: &RealFileCtx, id: &str, tty: bool, seq: &mut usize) {
    *seq += 1;
    let arg_digest = truncate(id, 32);
    let start = Instant::now();
    match ctx.api.file_nodes(&ctx.key, id) {
        Ok(_) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            print_api_line(
                *seq,
                "API node",
                &arg_digest,
                ms,
                false,
                "ok",
                "spent 1 Tier-1 call",
                tty,
            );
        }
        Err(ApiError::RateLimited { retry_after }) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let msg = format!("429 — retry after {}s", retry_after.as_secs());
            print_api_line(
                *seq,
                "API node",
                &arg_digest,
                ms,
                true,
                &msg,
                "spent 1 Tier-1 call (rate-limited)",
                tty,
            );
        }
        Err(e) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            print_api_line(
                *seq,
                "API node",
                &arg_digest,
                ms,
                true,
                &e.to_string(),
                "spent 1 Tier-1 call",
                tty,
            );
        }
    }
}

fn cmd_api_meta(ctx: &RealFileCtx, tty: bool, seq: &mut usize) {
    *seq += 1;
    let start = Instant::now();
    match ctx.api.file_meta(&ctx.key) {
        Ok(m) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            print_api_line(
                *seq,
                "API meta",
                "",
                ms,
                false,
                &m.name,
                "spent 1 Tier-3 call",
                tty,
            );
        }
        Err(ApiError::RateLimited { retry_after }) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let msg = format!("429 — retry after {}s", retry_after.as_secs());
            print_api_line(
                *seq,
                "API meta",
                "",
                ms,
                true,
                &msg,
                "spent 1 Tier-3 call (rate-limited)",
                tty,
            );
        }
        Err(e) => {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            print_api_line(
                *seq,
                "API meta",
                "",
                ms,
                true,
                &e.to_string(),
                "spent 1 Tier-3 call",
                tty,
            );
        }
    }
}

fn fire_and_print(session: &mut BenchSession, tool: &str, args: Value, tty: bool, seq: &mut usize) {
    *seq += 1;
    match session.fire(tool, args.clone()) {
        Ok((elapsed, resp)) => println!(
            "{}",
            format_latency_line(*seq, tool, &args, elapsed, &resp, tty)
        ),
        Err(e) => println!(
            "{}",
            paint(
                &format!("#{seq:>4}  {tool:<18} transport error: {e}"),
                Color::Red,
                tty
            )
        ),
    }
}

/// `run N`: fire N requests of the derived mixed workload, streaming one
/// line per request as it fires, then the burst's own percentile table.
fn run_burst(session: &mut BenchSession, n: usize, tty: bool, seq: &mut usize) {
    let start_idx = session.stats().len();
    for _ in 0..n {
        let (tool, args) = session.next_mixed_call();
        *seq += 1;
        match session.fire(tool, args.clone()) {
            Ok((elapsed, resp)) => println!(
                "{}",
                format_latency_line(*seq, tool, &args, elapsed, &resp, tty)
            ),
            Err(e) => {
                println!(
                    "{}",
                    paint(
                        &format!("#{seq:>4}  {tool:<18} transport error: {e}"),
                        Color::Red,
                        tty
                    )
                );
                break; // the child is gone; no point spamming N more errors
            }
        }
    }
    let burst = &session.stats()[start_idx..];
    println!();
    print_tool_table(&group_tool_stats(burst));
}

fn print_help() {
    println!("commands:");
    println!("  search <words…>            BM25 search over layer names/text");
    println!("  node <id> [children]       full node JSON");
    println!("  tree [id] [depth]          subtree outline");
    println!("  find <TYPE> [page]         nodes by Figma node type");
    println!("  where <pointer> [value]    nodes matching an RFC 6901 pointer");
    println!("  stats                      node counts, totals, max depth");
    println!("  path <id>                  ancestor chain to a node");
    println!("  text [page]                every TEXT node's characters");
    println!("  at <x> <y>                 nodes containing a point");
    println!("  instances <target>         instances of a component");
    println!("  components                 design-system inventory");
    println!("  styles [type]              styles with usage counts");
    println!("  uses <id>                  nodes using a style/variable id");
    println!("  vars [id]                  variables");
    println!("  pages                      list pages");
    println!("  status                     file name/version/node count");
    println!("  run <N>                    fire N requests of the mixed workload");
    println!("  report                     cumulative per-tool percentiles this session");
    println!("  api node <id> | api meta   real Figma API call (real-file mode only)");
    println!("  call <tool> <json-args>    raw escape hatch");
    println!("  help                       this text");
    println!("  quit                       exit (EOF also works)");
}

/// Drive the REPL to completion: reads lines from stdin until `quit` or
/// EOF, firing tool calls against `session` and (in real-file mode)
/// `real_file`'s API client. Never touches the child's lifecycle beyond
/// `session.fire` — the caller owns spawn/finish (`bench::run_interactive`).
pub fn run(session: &mut BenchSession, real_file: Option<RealFileCtx>) -> Result<(), String> {
    let tty = io::stdout().is_terminal();
    let stdin = io::stdin();
    let mut seq: usize = 0;

    println!("figmog bench --interactive — type `help` for commands, `quit` to exit.");

    loop {
        if tty {
            print!("figmog> ");
            io::stdout().flush().map_err(|e| e.to_string())?;
        }

        let mut line = String::new();
        let n = stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("reading stdin: {e}"))?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match parse_line(trimmed) {
            Err(msg) => println!("{}", paint(&format!("error: {msg}"), Color::Red, tty)),
            Ok(Command::Help) => print_help(),
            Ok(Command::Quit) => break,
            Ok(Command::Run(n)) => run_burst(session, n, tty, &mut seq),
            Ok(Command::Report) => {
                println!();
                print_tool_table(&group_tool_stats(session.stats()));
            }
            Ok(Command::Api(api_cmd)) => match &real_file {
                None => println!(
                    "api: real-file mode only — pass a Figma file to `figmog bench <file> --interactive`"
                ),
                Some(ctx) => match api_cmd {
                    ApiCmd::Node(id) => cmd_api_node(ctx, &id, tty, &mut seq),
                    ApiCmd::Meta => cmd_api_meta(ctx, tty, &mut seq),
                },
            },
            Ok(Command::Call { tool, args }) | Ok(Command::Tool { name: tool, args }) => {
                fire_and_print(session, &tool, args, tty, &mut seq);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_line: happy paths ----

    #[test]
    fn parses_help_quit_report() {
        assert_eq!(parse_line("help"), Ok(Command::Help));
        assert_eq!(parse_line("quit"), Ok(Command::Quit));
        assert_eq!(parse_line("report"), Ok(Command::Report));
    }

    #[test]
    fn parses_run_n() {
        assert_eq!(parse_line("run 20"), Ok(Command::Run(20)));
        assert_eq!(parse_line("  run   5  "), Ok(Command::Run(5)));
    }

    #[test]
    fn parses_api_node_and_meta() {
        assert_eq!(
            parse_line("api node 12:34"),
            Ok(Command::Api(ApiCmd::Node("12:34".to_string())))
        );
        assert_eq!(parse_line("api meta"), Ok(Command::Api(ApiCmd::Meta)));
    }

    #[test]
    fn parses_call_with_json_args() {
        assert_eq!(
            parse_line(r#"call figmog_node {"id":"12:34"}"#),
            Ok(Command::Call {
                tool: "figmog_node".to_string(),
                args: json!({"id": "12:34"}),
            })
        );
    }

    #[test]
    fn parses_call_with_no_args_defaults_to_empty_object() {
        assert_eq!(
            parse_line("call figmog_stats"),
            Ok(Command::Call {
                tool: "figmog_stats".to_string(),
                args: json!({}),
            })
        );
    }

    #[test]
    fn parses_every_shorthand_in_spec_13s_list() {
        assert_eq!(
            parse_line("search garden gnome"),
            Ok(Command::Tool {
                name: "figmog_search".to_string(),
                args: json!({"query": "garden gnome"}),
            })
        );
        assert_eq!(
            parse_line("node 12:34"),
            Ok(Command::Tool {
                name: "figmog_node".to_string(),
                args: json!({"id": "12:34"}),
            })
        );
        assert_eq!(
            parse_line("node 12:34 children"),
            Ok(Command::Tool {
                name: "figmog_node".to_string(),
                args: json!({"id": "12:34", "children": true}),
            })
        );
        assert_eq!(
            parse_line("tree"),
            Ok(Command::Tool {
                name: "figmog_tree".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("tree 0:0 2"),
            Ok(Command::Tool {
                name: "figmog_tree".to_string(),
                args: json!({"id": "0:0", "depth": 2}),
            })
        );
        assert_eq!(
            parse_line("find FRAME"),
            Ok(Command::Tool {
                name: "figmog_find".to_string(),
                args: json!({"type": "FRAME"}),
            })
        );
        assert_eq!(
            parse_line("find FRAME 1:0"),
            Ok(Command::Tool {
                name: "figmog_find".to_string(),
                args: json!({"type": "FRAME", "page": "1:0"}),
            })
        );
        assert_eq!(
            parse_line("where /layoutMode"),
            Ok(Command::Tool {
                name: "figmog_where".to_string(),
                args: json!({"pointer": "/layoutMode"}),
            })
        );
        assert_eq!(
            parse_line("where /layoutMode VERTICAL"),
            Ok(Command::Tool {
                name: "figmog_where".to_string(),
                args: json!({"pointer": "/layoutMode", "equals": "VERTICAL"}),
            })
        );
        assert_eq!(
            parse_line("where /width 100"),
            Ok(Command::Tool {
                name: "figmog_where".to_string(),
                args: json!({"pointer": "/width", "equals": 100}),
            })
        );
        assert_eq!(
            parse_line("stats"),
            Ok(Command::Tool {
                name: "figmog_stats".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("path 12:34"),
            Ok(Command::Tool {
                name: "figmog_path".to_string(),
                args: json!({"id": "12:34"}),
            })
        );
        assert_eq!(
            parse_line("text"),
            Ok(Command::Tool {
                name: "figmog_text".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("text 1:0"),
            Ok(Command::Tool {
                name: "figmog_text".to_string(),
                args: json!({"page": "1:0"}),
            })
        );
        assert_eq!(
            parse_line("at 100 200"),
            Ok(Command::Tool {
                name: "figmog_at".to_string(),
                args: json!({"x": 100.0, "y": 200.0}),
            })
        );
        assert_eq!(
            parse_line("instances Button"),
            Ok(Command::Tool {
                name: "figmog_instances".to_string(),
                args: json!({"target": "Button"}),
            })
        );
        assert_eq!(
            parse_line("components"),
            Ok(Command::Tool {
                name: "figmog_components".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("styles"),
            Ok(Command::Tool {
                name: "figmog_styles".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("styles FILL"),
            Ok(Command::Tool {
                name: "figmog_styles".to_string(),
                args: json!({"type": "FILL"}),
            })
        );
        assert_eq!(
            parse_line("uses S:1"),
            Ok(Command::Tool {
                name: "figmog_uses".to_string(),
                args: json!({"id": "S:1"}),
            })
        );
        assert_eq!(
            parse_line("vars"),
            Ok(Command::Tool {
                name: "figmog_vars".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("vars VariableID:1"),
            Ok(Command::Tool {
                name: "figmog_vars".to_string(),
                args: json!({"id": "VariableID:1"}),
            })
        );
        assert_eq!(
            parse_line("pages"),
            Ok(Command::Tool {
                name: "figmog_pages".to_string(),
                args: json!({}),
            })
        );
        assert_eq!(
            parse_line("status"),
            Ok(Command::Tool {
                name: "figmog_status".to_string(),
                args: json!({}),
            })
        );
    }

    // ---- parse_line: bad input ----

    #[test]
    fn empty_line_is_an_error() {
        assert!(parse_line("").is_err());
        assert!(parse_line("   ").is_err());
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert!(parse_line("frobnicate").is_err());
    }

    #[test]
    fn missing_required_args_are_errors() {
        assert!(parse_line("node").is_err());
        assert!(parse_line("search").is_err());
        assert!(parse_line("find").is_err());
        assert!(parse_line("where").is_err());
        assert!(parse_line("path").is_err());
        assert!(parse_line("at").is_err());
        assert!(parse_line("at 1").is_err());
        assert!(parse_line("instances").is_err());
        assert!(parse_line("uses").is_err());
        assert!(parse_line("run").is_err());
        assert!(parse_line("api").is_err());
        assert!(parse_line("api node").is_err());
        assert!(parse_line("call").is_err());
    }

    #[test]
    fn non_numeric_run_and_at_and_tree_depth_are_errors() {
        assert!(parse_line("run abc").is_err());
        assert!(parse_line("at abc 200").is_err());
        assert!(parse_line("at 100 abc").is_err());
        assert!(parse_line("tree 0:0 abc").is_err());
    }

    #[test]
    fn invalid_json_call_args_is_an_error() {
        assert!(parse_line("call figmog_node {not json}").is_err());
    }

    #[test]
    fn unknown_api_subcommand_is_an_error() {
        assert!(parse_line("api bogus").is_err());
    }

    // ---- latency-line formatter (plain mode) ----

    fn ok_resp(text: &str) -> Value {
        json!({"result": {"content": [{"type": "text", "text": text}], "isError": false}})
    }

    fn err_resp(text: &str) -> Value {
        json!({"result": {"content": [{"type": "text", "text": text}], "isError": true}})
    }

    #[test]
    fn plain_mode_has_no_ansi_bytes() {
        let resp = ok_resp(r#"[{"id":"1"},{"id":"2"}]"#);
        let line = format_latency_line(
            1,
            "figmog_search",
            &json!({"query": "hi"}),
            Duration::from_millis(3),
            &resp,
            false,
        );
        assert!(
            !line.contains('\x1b'),
            "plain mode must emit zero ANSI bytes: {line:?}"
        );
    }

    #[test]
    fn array_result_digest_is_hit_count() {
        let resp = ok_resp(r#"[{"id":"1"},{"id":"2"},{"id":"3"}]"#);
        let line = format_latency_line(
            1,
            "figmog_search",
            &json!({"query": "hi"}),
            Duration::from_millis(3),
            &resp,
            false,
        );
        assert!(
            line.contains("3 hits"),
            "expected a hit count digest: {line:?}"
        );
    }

    #[test]
    fn named_object_result_digest_is_the_name() {
        let resp = ok_resp(r#"{"id":"12:34","name":"Button Frame","type":"FRAME"}"#);
        let line = format_latency_line(
            1,
            "figmog_node",
            &json!({"id": "12:34"}),
            Duration::from_millis(3),
            &resp,
            false,
        );
        assert!(
            line.contains("Button Frame"),
            "expected the node's name as digest: {line:?}"
        );
    }

    #[test]
    fn error_result_digest_is_the_error_text() {
        let resp = err_resp("unknown node: 99:99");
        let line = format_latency_line(
            1,
            "figmog_node",
            &json!({"id": "99:99"}),
            Duration::from_millis(3),
            &resp,
            false,
        );
        assert!(
            line.contains("unknown node: 99:99"),
            "expected the isError text as digest: {line:?}"
        );
    }

    #[test]
    fn args_digest_is_truncated_to_32_chars() {
        let long_query = "a".repeat(64);
        let resp = ok_resp("[]");
        let line = format_latency_line(
            1,
            "figmog_search",
            &json!({"query": long_query}),
            Duration::from_millis(1),
            &resp,
            false,
        );
        // The compact JSON of {"query": "aaa...a"} is longer than 64 chars
        // itself; just assert the 65-a run got cut down well below its
        // untruncated length so it can't have been emitted whole.
        assert!(
            !line.contains(&"a".repeat(60)),
            "args digest should be truncated: {line:?}"
        );
    }

    #[test]
    fn seq_and_tool_and_ms_are_formatted_and_aligned() {
        let resp = ok_resp("[]");
        let line = format_latency_line(
            7,
            "figmog_stats",
            &json!({}),
            Duration::from_micros(1500),
            &resp,
            false,
        );
        assert!(
            line.starts_with("#   7  figmog_stats"),
            "unexpected line prefix: {line:?}"
        );
        assert!(
            line.contains("1.50ms"),
            "expected a 2-decimal ms value: {line:?}"
        );
    }

    // ---- group_tool_stats reachability check (used by run/report) ----

    #[test]
    fn group_tool_stats_preserves_first_appearance_order() {
        let entries = vec![
            ("figmog_search".to_string(), Duration::from_millis(1)),
            ("figmog_node".to_string(), Duration::from_millis(2)),
            ("figmog_search".to_string(), Duration::from_millis(3)),
        ];
        let stats = group_tool_stats(&entries);
        let names: Vec<&str> = stats.iter().map(|t| t.tool.as_str()).collect();
        assert_eq!(names, vec!["figmog_search", "figmog_node"]);
        assert_eq!(stats[0].calls, 2);
        assert_eq!(stats[1].calls, 1);
    }
}
