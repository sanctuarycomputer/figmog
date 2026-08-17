#![recursion_limit = "256"]

mod common;

use figmog::model::{Id, Rec};
use figmog::vars::parse_variables_export;

fn export() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/variables-export.json")).unwrap()
}

#[test]
fn parses_rest_shape() {
    let recs = parse_variables_export(&export()).unwrap();
    // 2 collections then 3 variables, sorted by id
    assert_eq!(recs.len(), 5);
    assert!(matches!(&recs[0].0, Id::VariableCollection(id) if id == "VariableCollectionId:1"));
    let Rec::VariableCollection(c) = &recs[0].1 else {
        panic!()
    };
    assert_eq!(
        c.modes,
        vec![
            ("1:0".to_string(), "light".to_string()),
            ("1:1".to_string(), "dark".to_string())
        ]
    );
    assert_eq!(c.default_mode_id, "1:0");

    let Rec::Variable(v) = &recs[2].1 else {
        panic!()
    };
    assert_eq!(v.id, "VariableID:100");
    assert_eq!(v.resolved_type, "COLOR");
    assert_eq!(v.collection_id, "VariableCollectionId:1");
    // values canonical JSON, sorted by mode id; alias kept as-is
    assert_eq!(v.values_by_mode[0].0, "1:0");
    assert!(v.values_by_mode[1].1.contains("VARIABLE_ALIAS"));
    assert_eq!(v.scopes, vec!["FRAME_FILL", "SHAPE_FILL"]);
}

#[test]
fn accepts_bare_shape_and_is_deterministic() {
    let bare = export()["meta"].clone();
    let a = parse_variables_export(&bare).unwrap();
    let b = parse_variables_export(&export()).unwrap();
    assert_eq!(
        postcard::to_allocvec(&a).unwrap(),
        postcard::to_allocvec(&b).unwrap(),
        "both shapes produce byte-identical records"
    );
}

#[test]
fn garbage_is_a_shape_error() {
    assert!(parse_variables_export(&serde_json::json!({"nope": 1})).is_err());
    assert!(parse_variables_export(&serde_json::json!(null)).is_err());
}

use figmog::flatten::flatten_file;
use figmog::vars::{infer_from_nodes, style_value_from_consumer};

#[test]
fn infers_values_and_sites_from_fixture() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let nodes: Vec<figmog::model::NodeRec> = out
        .recs
        .iter()
        .filter_map(|(_, r)| match r {
            Rec::Node(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let usages = infer_from_nodes(nodes.iter());

    assert_eq!(usages.len(), 2, "two distinct variables bound in fixture");
    let color = usages
        .iter()
        .find(|u| u.variable_id == "VariableID:100")
        .unwrap();
    assert_eq!(
        color.sites,
        vec![("1:1".to_string(), "/fills/0/color".to_string())]
    );
    let observed: serde_json::Value = serde_json::from_str(&color.observed[0]).unwrap();
    assert_eq!(observed["r"], 0.06);

    let pad = usages
        .iter()
        .find(|u| u.variable_id == "VariableID:200")
        .unwrap();
    assert_eq!(pad.observed, vec!["16.0".to_string()]);
}

#[test]
fn style_values_come_from_consumers() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = out
        .recs
        .iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Node(id), Rec::Node(n)) if id == "1:2" => Some(n.clone()),
            _ => None,
        })
        .unwrap();
    let v = style_value_from_consumer("TEXT", &title.raw).unwrap();
    assert_eq!(v["fontSize"], 32.0);
    assert!(
        style_value_from_consumer("FILL", &title.raw).is_none(),
        "no fills on the text node"
    );
}
