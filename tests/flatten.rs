#![recursion_limit = "256"]

mod common;

use figmog::flatten::flatten_file;
use figmog::model::{ComponentRec, Id, Rec};

fn node(recs: &[(Id, Rec)], id: &str) -> figmog::model::NodeRec {
    recs.iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Node(n), Rec::Node(rec)) if n == id => Some(rec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("node {id} not flattened"))
}

fn component(recs: &[(Id, Rec)], id: &str) -> ComponentRec {
    recs.iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Component(n), Rec::Component(rec)) if n == id => Some(rec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("component {id} not flattened"))
}

#[test]
fn walks_the_whole_tree() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let node_ids: Vec<&str> = out
        .recs
        .iter()
        .filter_map(|(k, _)| match k {
            Id::Node(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        node_ids,
        [
            "0:0", "0:1", "1:1", "1:2", "1:3", "1:9", "0:2", "2:1", "2:2", "2:3", "3:1", "0:3"
        ],
        "depth-first order, all 12 nodes"
    );
    assert_eq!(out.file.name, "Fixture");
    assert_eq!(out.file.version, "100");
    assert_eq!(out.file.last_modified, "2026-08-01T00:00:00Z");
}

#[test]
fn parent_index_page_attribution() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = node(&out.recs, "1:2");
    assert_eq!(title.parent_id.as_deref(), Some("1:1"));
    assert_eq!(title.child_index, 0);
    assert_eq!(title.page_id, "0:1");
    let button = node(&out.recs, "1:3");
    assert_eq!(button.child_index, 1);

    let root = node(&out.recs, "0:0");
    assert_eq!(root.parent_id, None);
    assert_eq!(root.page_id, "0:0");
    let canvas = node(&out.recs, "0:2");
    assert_eq!(canvas.page_id, "0:2");
    let variant = node(&out.recs, "2:2");
    assert_eq!(variant.page_id, "0:2");
}

#[test]
fn basic_fields() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = node(&out.recs, "1:2");
    assert_eq!(title.node_type, "TEXT");
    assert_eq!(title.name, "Title");
    assert!(title.visible);
    assert_eq!(title.text.as_deref(), Some("Welcome to the garden"));

    let hidden = node(&out.recs, "1:9");
    assert!(!hidden.visible);

    let hero = node(&out.recs, "1:1");
    assert_eq!(hero.abs_bounds, Some([0.0, 0.0, 800.0, 400.0]));
}

#[test]
fn raw_is_canonical_and_childless() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let hero = node(&out.recs, "1:1");
    let raw: serde_json::Value = serde_json::from_str(&hero.raw).unwrap();
    assert!(raw.get("children").is_none());
    assert_eq!(raw["name"], "Hero");
    // canonical: re-serializing the parsed value reproduces the string
    assert_eq!(serde_json::to_string(&raw).unwrap(), hero.raw);
}

#[test]
fn deterministic_bytes() {
    let a = flatten_file(&common::fixture_v1()).unwrap();
    let b = flatten_file(&common::fixture_v1()).unwrap();
    let enc = |f: &figmog::flatten::Flattened| postcard::to_allocvec(&f.recs).unwrap();
    assert_eq!(enc(&a), enc(&b));
}

#[test]
fn missing_document_errors() {
    assert!(flatten_file(&serde_json::json!({"name": "x"})).is_err());
}

#[test]
fn instance_component_fields() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let button = node(&out.recs, "1:3");
    assert_eq!(button.component_id.as_deref(), Some("2:2"));
    // sorted by property name; values are canonical JSON of the `value` field
    assert_eq!(
        button.component_properties,
        vec![
            ("HasIcon".to_string(), "false".to_string()),
            ("Icon".to_string(), "\"3:1\"".to_string()),
            ("Label".to_string(), "\"Go\"".to_string()),
            ("Size".to_string(), "\"Large\"".to_string()),
            ("State".to_string(), "\"Default\"".to_string()),
        ]
    );
}

#[test]
fn property_definitions_on_set_and_component() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let set = node(&out.recs, "2:1");
    let defs: serde_json::Value =
        serde_json::from_str(set.property_definitions.as_deref().unwrap()).unwrap();
    assert_eq!(
        defs["Size"]["variantOptions"],
        serde_json::json!(["Large", "Small"])
    );
    // standalone component without the field -> None
    assert_eq!(node(&out.recs, "3:1").property_definitions, None);
}

#[test]
fn style_refs_extracted_sorted() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    assert_eq!(
        node(&out.recs, "1:1").style_refs,
        vec![("fill".to_string(), "S:1".to_string())]
    );
    assert_eq!(
        node(&out.recs, "1:2").style_refs,
        vec![("text".to_string(), "S:2".to_string())]
    );
}

#[test]
fn bound_variable_scan_finds_all_depths() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let hero = node(&out.recs, "1:1");
    // sorted by pointer; pointer addresses the RESOLVED value location
    assert_eq!(
        hero.bound_variables,
        vec![
            ("/fills/0/color".to_string(), "VariableID:100".to_string()),
            ("/paddingLeft".to_string(), "VariableID:200".to_string()),
        ]
    );
}

#[test]
fn envelope_maps_flattened() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let c = component(&out.recs, "2:2");
    assert_eq!(c.key, "key22");
    assert_eq!(c.component_set_id.as_deref(), Some("2:1"));
    assert!(!c.remote);

    let styles: Vec<figmog::model::StyleRec> = out
        .recs
        .iter()
        .filter_map(|(_, r)| match r {
            Rec::Style(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(styles.len(), 2);
    assert_eq!(styles[0].style_id, "S:1"); // sorted by style id
    assert_eq!(styles[0].style_type, "FILL");

    let sets = out
        .recs
        .iter()
        .filter(|(k, _)| matches!(k, Id::ComponentSet(_)))
        .count();
    assert_eq!(sets, 1);
}
