//! Synthetic Figma file fixtures. Deliberately NOT derived from any real
//! file. Shape mirrors GET /v1/files/:key responses.

use assert_cmd::Command;
use serde_json::{Value, json};

/// 12 nodes over 3 pages: a hero frame with a text, a variant'd button
/// instance, an invisible node, a component set (2 variants), a standalone
/// component, and an empty page. Fill/text styles + variable bindings.
pub fn fixture_v1() -> Value {
    json!({
        "name": "Fixture",
        "version": "100",
        "lastModified": "2026-08-01T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [
                { "id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                    { "id": "1:1", "name": "Hero", "type": "FRAME",
                      "absoluteBoundingBox": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 400.0},
                      "layoutMode": "VERTICAL", "paddingLeft": 16.0,
                      "boundVariables": { "paddingLeft": {"type": "VARIABLE_ALIAS", "id": "VariableID:200"} },
                      "fills": [ { "type": "SOLID",
                                   "color": {"r": 0.06, "g": 0.13, "b": 0.2, "a": 1.0},
                                   "boundVariables": { "color": {"type": "VARIABLE_ALIAS", "id": "VariableID:100"} } } ],
                      "styles": { "fill": "S:1" },
                      "children": [
                        { "id": "1:2", "name": "Title", "type": "TEXT",
                          "characters": "Welcome to the garden",
                          "style": {"fontFamily": "Basis", "fontSize": 32.0, "fontWeight": 500},
                          "styles": { "text": "S:2" },
                          "children": [] },
                        { "id": "1:3", "name": "Button", "type": "INSTANCE",
                          "componentId": "2:2",
                          "componentProperties": {
                              "Size": {"value": "Large", "type": "VARIANT"},
                              "State": {"value": "Default", "type": "VARIANT"},
                              "Label": {"value": "Go", "type": "TEXT"},
                              "HasIcon": {"value": false, "type": "BOOLEAN"},
                              "Icon": {"value": "3:1", "type": "INSTANCE_SWAP"}
                          },
                          "children": [] }
                      ] },
                    { "id": "1:9", "name": "Old badge", "type": "RECTANGLE",
                      "visible": false, "children": [] }
                ] },
                { "id": "0:2", "name": "Components", "type": "CANVAS", "children": [
                    { "id": "2:1", "name": "Button", "type": "COMPONENT_SET",
                      "componentPropertyDefinitions": {
                          "Size": {"type": "VARIANT", "defaultValue": "Large", "variantOptions": ["Large", "Small"]},
                          "State": {"type": "VARIANT", "defaultValue": "Default", "variantOptions": ["Default", "Hover"]},
                          "Label": {"type": "TEXT", "defaultValue": "Go"},
                          "HasIcon": {"type": "BOOLEAN", "defaultValue": false},
                          "Icon": {"type": "INSTANCE_SWAP", "defaultValue": "3:1"}
                      },
                      "children": [
                        { "id": "2:2", "name": "Size=Large, State=Default", "type": "COMPONENT", "children": [] },
                        { "id": "2:3", "name": "Size=Small, State=Hover", "type": "COMPONENT", "children": [] }
                      ] },
                    { "id": "3:1", "name": "IconStar", "type": "COMPONENT", "children": [] }
                ] },
                { "id": "0:3", "name": "Empty", "type": "CANVAS", "children": [] }
            ]
        },
        "components": {
            "2:2": {"key": "key22", "name": "Size=Large, State=Default", "description": "", "componentSetId": "2:1", "remote": false},
            "2:3": {"key": "key23", "name": "Size=Small, State=Hover", "description": "", "componentSetId": "2:1", "remote": false},
            "3:1": {"key": "key31", "name": "IconStar", "description": "a star", "remote": false}
        },
        "componentSets": {
            "2:1": {"key": "keyset21", "name": "Button", "description": "the button", "remote": false}
        },
        "styles": {
            "S:1": {"key": "sk1", "name": "Brand/Primary", "styleType": "FILL", "description": "", "remote": false},
            "S:2": {"key": "sk2", "name": "Heading/H1", "styleType": "TEXT", "description": "", "remote": false}
        }
    })
}

/// v1 plus: rename 1:2, delete 1:9, add 1:4, repoint instance 1:3 at the
/// Small variant, bump version.
#[allow(dead_code)] // not every test binary uses v2
pub fn fixture_v2() -> Value {
    let mut v = fixture_v1();
    v["version"] = json!("101");
    v["lastModified"] = json!("2026-08-02T00:00:00Z");
    let page1 = &mut v["document"]["children"][0];
    // delete 1:9 (second child of the canvas)
    page1["children"].as_array_mut().unwrap().remove(1);
    let hero = &mut page1["children"][0];
    hero["children"][0]["name"] = json!("Headline");
    hero["children"][1]["componentId"] = json!("2:3");
    hero["children"][1]["componentProperties"]["Size"]["value"] = json!("Small");
    hero["children"][1]["componentProperties"]["State"]["value"] = json!("Hover");
    hero["children"].as_array_mut().unwrap().push(json!({
        "id": "1:4", "name": "Subtitle", "type": "TEXT",
        "characters": "Planting season", "children": []
    }));
    v
}

/// A second, small, distinct fixture — used by the multi-file `serve` e2e
/// (`tests/serve.rs`) to prove `file`-argument routing actually reaches a
/// *different* mirror rather than always answering from the first one.
/// Deliberately tiny (3 nodes) and textually disjoint from [`fixture_v1`]:
/// its one TEXT node's `characters` contains "zephyr", a word that appears
/// nowhere in `fixture_v1`, so a search hit for it proves routing.
#[allow(dead_code)] // not every test binary that includes this module calls it
pub fn fixture_other() -> Value {
    json!({
        "name": "OtherFixture",
        "version": "1",
        "lastModified": "2026-08-03T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [
                { "id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                    { "id": "1:1", "name": "Banner", "type": "TEXT",
                      "characters": "Feel the zephyr breeze",
                      "children": [] }
                ] }
            ]
        },
        "components": {},
        "componentSets": {},
        "styles": {}
    })
}

/// Materialize [`fixture_v1`] into a DB via `pull --from-file` and return the
/// (tempdir, db-path) pair every read command — CLI or `serve` — needs.
/// Shared so `tests/cli.rs` and `tests/serve.rs` build the same fixture the
/// same way.
#[allow(dead_code)] // not every test binary that includes this module calls it
pub fn fixture_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(&response, serde_json::to_string(&fixture_v1()).unwrap()).unwrap();
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
    (dir, db)
}
