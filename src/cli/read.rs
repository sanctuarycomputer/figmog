//! The read-only query commands (`status`, `pages`, `tree`, ... `at`).
//!
//! Every command below prints its `query::*` `Value` as pretty-printed
//! JSON on stdout — the only output mode (spec §4). Failures propagate as
//! `Err(String)`, which `run`'s top-level handler renders as `{"error":
//! ...}` on stderr with exit 1.

use serde_json::Value;

use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;

use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, NodeRec, StyleRec, VariableCollectionRec, VariableRec,
};
use crate::query::{self, TextReader};

use super::write_json;

fn print_value(v: &Value) -> Result<(), String> {
    write_json(v)
}

pub(super) fn cmd_status<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
) -> Result<(), String> {
    print_value(&query::status(nodes, meta)?)
}

pub(super) fn cmd_pages<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<(), String> {
    print_value(&query::pages(nodes, by_type)?)
}

pub(super) fn cmd_tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<(), String> {
    print_value(&query::tree(nodes, children, by_type, id, depth)?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_get<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    with_children: bool,
    resolve_vars: bool,
) -> Result<(), String> {
    print_value(&query::node(
        nodes,
        children,
        variables,
        variable_collections,
        id,
        with_children,
        resolve_vars,
    )?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_dump<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    depth: Option<usize>,
    fields: Option<Vec<String>>,
    resolve_vars: bool,
) -> Result<(), String> {
    print_value(&query::subtree(
        nodes,
        children,
        variables,
        variable_collections,
        id,
        depth,
        fields.as_deref(),
        resolve_vars,
    )?)
}

pub(super) fn cmd_find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
    under: Option<String>,
) -> Result<(), String> {
    print_value(&query::find(
        nodes, children, by_type, node_type, page, under,
    )?)
}

// ---- design-system reads ----

pub(super) fn cmd_search<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    text: &TextReader<'_, R>,
    query: String,
    limit: usize,
    under: Option<String>,
) -> Result<(), String> {
    print_value(&query::search(text, nodes, children, &query, limit, under)?)
}

pub(super) fn cmd_instances<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    instances_of: &InvertedIndexReader<'_, R, String, String>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    target: String,
) -> Result<(), String> {
    print_value(&query::instances(
        nodes,
        components,
        component_sets,
        instances_of,
        &target,
    )?)
}

pub(super) fn cmd_components<R: Readable>(
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    nodes: &TableReader<'_, R, String, NodeRec>,
) -> Result<(), String> {
    print_value(&query::components(nodes, components, component_sets)?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_styles<R: Readable>(
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    style_type: Option<String>,
    values: bool,
    resolve_vars: bool,
) -> Result<(), String> {
    print_value(&query::styles(
        nodes,
        styles,
        styled_by,
        variables,
        variable_collections,
        style_type,
        values,
        resolve_vars,
    )?)
}

pub(super) fn cmd_uses<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    bound_to: &InvertedIndexReader<'_, R, String, String>,
    id: String,
) -> Result<(), String> {
    print_value(&query::uses(nodes, styled_by, bound_to, &id)?)
}

pub(super) fn cmd_vars<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: Option<String>,
) -> Result<(), String> {
    print_value(&query::vars(nodes, variables, variable_collections, id)?)
}

// ---- whole-file structural queries ----

/// `--equals <json>`: parse as JSON, falling back to treating the bare word
/// as a JSON string (so `--equals VERTICAL` works without quoting).
fn parse_equals(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

pub(super) fn cmd_stats<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<(), String> {
    print_value(&query::stats(
        nodes,
        components,
        component_sets,
        styles,
        variables,
        by_type,
    )?)
}

pub(super) fn cmd_path<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    id: String,
) -> Result<(), String> {
    print_value(&query::path(nodes, id)?)
}

pub(super) fn cmd_text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
    under: Option<String>,
) -> Result<(), String> {
    print_value(&query::text(nodes, children, by_type, page, under)?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_where<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    pointer: String,
    equals: Option<String>,
    page: Option<String>,
    under: Option<String>,
) -> Result<(), String> {
    let equals = equals.as_deref().map(parse_equals);
    print_value(&query::where_(
        nodes, children, &pointer, equals, page, under,
    )?)
}

pub(super) fn cmd_at<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    x: f64,
    y: f64,
) -> Result<(), String> {
    print_value(&query::at(nodes, x, y)?)
}
