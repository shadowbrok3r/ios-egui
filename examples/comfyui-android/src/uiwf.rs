//! Converts ComfyUI **UI-format** workflow JSON (what the web frontend saves: `nodes`, `links`,
//! `widgets_values`) into an API-format [`Workflow`]. Widget ordering and value typing come from
//! the lenient [`SchemaSet`]. Handles Reroute chains, mode-4 bypass splicing, legacy
//! `PrimitiveNode` inlining, KJNodes `SetNode`/`GetNode` invisible wires,
//! `control_after_generate` phantom values, and both array- and dict-form `widgets_values`.

use std::collections::{BTreeMap, HashMap};

use rucomfyui::Workflow;
use rucomfyui::workflow::{WorkflowInput, WorkflowMeta, WorkflowNode, WorkflowNodeId};
use serde_json::Value;

use crate::schema::{InputKind, InputSchema, NodeSchema, SchemaSet};

/// Frontend-only node types that never reach the server.
const VIRTUAL: &[&str] = &["Reroute", "Note", "MarkdownNote", "PrimitiveNode", "SetNode", "GetNode"];
/// The phantom widget value the frontend stores after a seed widget.
const SEED_CONTROLS: &[&str] = &["fixed", "increment", "decrement", "randomize"];
/// Dict-form `widgets_values` keys that are frontend state, not node inputs.
const UI_ONLY_WIDGETS: &[&str] = &["videopreview", "control_after_generate"];
/// Bypassed nodes (frontend splices matching-type links straight through them).
const MODE_BYPASS: u64 = 4;
/// Muted nodes (produce nothing).
const MODE_MUTE: u64 = 2;

pub struct Converted {
    pub workflow: Workflow,
    pub warnings: Vec<String>,
    /// UI/API node id → seed input name → randomize (`control_after_generate == "randomize"`).
    pub seed_randomize: BTreeMap<(u64, String), bool>,
    /// UI/API node id → input name → value, for widgets `/object_info` doesn't declare. The editor
    /// builds its widgets from the schema, so these have to ride alongside it to reach the wire.
    pub extra_widgets: BTreeMap<(u64, String), Value>,
}

/// Where a link's value ultimately comes from after skipping virtual/bypassed nodes.
enum Source {
    Real { node: u64, slot: u32 },
    Primitive { node: u64 },
    Lost,
}

pub fn convert(ui: &Value, schemas: &SchemaSet) -> Result<Converted, String> {
    let mut warnings = Vec::new();
    let flat = flatten_subgraphs(ui, schemas, &mut warnings);
    let ui = flat.as_ref().unwrap_or(ui);

    let nodes = ui
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or("no `nodes` array — not a UI-format workflow")?;

    let by_id: BTreeMap<u64, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n.get("id")?.as_u64()?, n)))
        .collect();
    let links = link_table(ui);
    let set_nodes = set_node_table(&by_id);

    let mut out: BTreeMap<WorkflowNodeId, WorkflowNode> = BTreeMap::new();
    let mut seed_randomize: BTreeMap<(u64, String), bool> = BTreeMap::new();
    let mut extra_widgets: BTreeMap<(u64, String), Value> = BTreeMap::new();

    for (&id, node) in &by_id {
        let ty = node_type(node);
        if VIRTUAL.contains(&ty) {
            continue;
        }
        match node_mode(node) {
            MODE_BYPASS => continue,
            MODE_MUTE => {
                warnings.push(format!("muted node {id} ({ty}) dropped"));
                continue;
            }
            _ => {}
        }
        let Some(schema) = schemas.nodes.get(ty) else {
            warnings.push(format!("unknown node type `{ty}` (id {id}) skipped"));
            continue;
        };
        let Ok(wf_id) = u32::try_from(id) else {
            warnings.push(format!("node id {id} out of range, skipped"));
            continue;
        };

        let title = node
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&schema.display_name);
        let mut wnode = WorkflowNode::new(ty).with_meta(WorkflowMeta::new(title));

        // Widget values in schema order; connections below override converted widgets.
        let mut values = WidgetValues::new(node.get("widgets_values"));
        for input in schema.inputs.iter().filter(|i| is_widget(&i.kind)) {
            let (value, control) = values.next_for(input);
            if let Some(c) = control {
                seed_randomize.insert((id, input.name.clone()), c == "randomize");
            }
            let coerced = value
                .and_then(|v| coerce(&input.kind, v))
                .or_else(|| default_input(&input.kind));
            if let Some(wi) = coerced {
                wnode.add_input(input.name.clone(), wi);
            }
        }
        if let Some(extra) = values.leftover() {
            warnings.push(format!("{ty} (id {id}): {extra} unused widget value(s)"));
        }
        // The VideoHelperSuite family stores widgets by name and carries format-dependent widgets
        // (crf, pix_fmt, save_metadata) that `/object_info` never declares. Reconstructing the node
        // from known keys alone drops those, and it silently re-encodes at its own defaults.
        for (name, v) in values.undeclared(schema) {
            if !wnode.inputs.contains_key(name)
                && let Some(wi) = crate::preflight::input_of(v)
            {
                wnode.add_input(name.clone(), wi);
                extra_widgets.insert((id, name.clone()), v.clone());
            }
        }

        // Connected inputs.
        for entry in node.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = entry.get("name").and_then(Value::as_str) else { continue };
            let Some(link_id) = entry.get("link").and_then(Value::as_u64) else { continue };
            let Some(&(from_node, from_slot)) = links.get(&link_id) else { continue };
            match resolve(&by_id, &links, &set_nodes, from_node, from_slot) {
                Source::Real { node, slot } => {
                    wnode.add_input(name.to_string(), WorkflowInput::Slot(node.to_string(), slot));
                }
                Source::Primitive { node } => {
                    let value = by_id
                        .get(&node)
                        .and_then(|p| p.get("widgets_values"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());
                    let kind = schema
                        .inputs
                        .iter()
                        .find(|i| i.name == name)
                        .map(|i| &i.kind)
                        .unwrap_or(&InputKind::Opaque);
                    match value.and_then(|v| coerce_or_text(kind, v)) {
                        Some(wi) => wnode.add_input(name.to_string(), wi),
                        None => warnings.push(format!(
                            "{ty} (id {id}): primitive feeding `{name}` had no usable value"
                        )),
                    }
                }
                Source::Lost => {
                    warnings.push(format!(
                        "{ty} (id {id}): input `{name}` lost its source (muted or broken chain)"
                    ));
                }
            }
        }

        out.insert(WorkflowNodeId(wf_id), wnode);
    }

    if out.is_empty() {
        return Err("workflow contained no queueable nodes".into());
    }

    // Slot inputs pointing at skipped nodes (unknown types, out-of-range ids) would fail server
    // validation with an opaque error; drop them and say so.
    let mut dangling: Vec<(WorkflowNodeId, String)> = Vec::new();
    for (&id, node) in &out {
        for (name, input) in &node.inputs {
            let WorkflowInput::Slot(target, _) = input else { continue };
            if target.parse::<WorkflowNodeId>().map_or(true, |t| !out.contains_key(&t)) {
                dangling.push((id, name.clone()));
            }
        }
    }
    for (id, name) in dangling {
        warnings.push(format!("node {id}: input `{name}` dropped (its source was skipped)"));
        if let Some(node) = out.get_mut(&id) {
            node.inputs.remove(&name);
        }
    }

    Ok(Converted {
        workflow: Workflow::new(out),
        warnings,
        seed_randomize,
        extra_widgets,
    })
}

/// `links` rows are `[id, from_node, from_slot, to_node, to_slot, type]` (or objects in newer
/// litegraph exports).
fn link_table(ui: &Value) -> BTreeMap<u64, (u64, u32)> {
    let mut map = BTreeMap::new();
    for row in ui.get("links").and_then(Value::as_array).into_iter().flatten() {
        match row {
            Value::Array(a) if a.len() >= 3 => {
                if let (Some(id), Some(from), Some(slot)) =
                    (a[0].as_u64(), a[1].as_u64(), a[2].as_u64())
                {
                    map.insert(id, (from, slot as u32));
                }
            }
            Value::Object(o) => {
                if let (Some(id), Some(from), Some(slot)) = (
                    o.get("id").and_then(Value::as_u64),
                    o.get("origin_id").and_then(Value::as_u64),
                    o.get("origin_slot").and_then(Value::as_u64),
                ) {
                    map.insert(id, (from, slot as u32));
                }
            }
            _ => {}
        }
    }
    map
}

/// KJNodes `SetNode` ids by their name widget, for `GetNode` resolution.
fn set_node_table(by_id: &BTreeMap<u64, &Value>) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for (&id, node) in by_id {
        if node_type(node) == "SetNode"
            && let Some(name) = first_widget_str(node)
        {
            map.insert(name.to_string(), id);
        }
    }
    map
}

fn first_widget_str(node: &Value) -> Option<&str> {
    node.get("widgets_values").and_then(Value::as_array).and_then(|a| a.first())?.as_str()
}

/// Walk backwards through Reroutes, Set/GetNode wires, and bypassed nodes to the real producer.
fn resolve(
    by_id: &BTreeMap<u64, &Value>,
    links: &BTreeMap<u64, (u64, u32)>,
    set_nodes: &BTreeMap<String, u64>,
    mut node_id: u64,
    mut slot: u32,
) -> Source {
    for _ in 0..64 {
        let Some(node) = by_id.get(&node_id) else { return Source::Lost };
        let ty = node_type(node);
        let mode = node_mode(node);
        if ty == "PrimitiveNode" {
            return Source::Primitive { node: node_id };
        }
        if ty == "GetNode" {
            // Invisible wire: jump to the SetNode with the same name widget.
            match first_widget_str(node).and_then(|n| set_nodes.get(n)) {
                Some(&set_id) => {
                    node_id = set_id;
                    slot = 0;
                    continue;
                }
                None => return Source::Lost,
            }
        }
        if ty == "Reroute" || ty == "SetNode" || mode == MODE_BYPASS {
            // Bypass splices each output to the first linked input of the same type;
            // Reroute and SetNode have a single passthrough input.
            let want = if ty == "Reroute" || ty == "SetNode" {
                "*".to_string()
            } else {
                node.get("outputs")
                    .and_then(Value::as_array)
                    .and_then(|o| o.get(slot as usize))
                    .and_then(|o| o.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("*")
                    .to_string()
            };
            let next = node
                .get("inputs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find_map(|inp| {
                    let link = inp.get("link").and_then(Value::as_u64)?;
                    let ity = inp.get("type").and_then(Value::as_str).unwrap_or("*");
                    (want == "*" || ity == "*" || ity == want).then_some(link)
                })
                .and_then(|l| links.get(&l));
            match next {
                Some(&(n, s)) => {
                    node_id = n;
                    slot = s;
                }
                None => return Source::Lost,
            }
            continue;
        }
        if mode == MODE_MUTE {
            return Source::Lost;
        }
        return Source::Real { node: node_id, slot };
    }
    Source::Lost
}

fn node_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn node_mode(v: &Value) -> u64 {
    v.get("mode").and_then(Value::as_u64).unwrap_or(0)
}

pub fn is_widget(kind: &InputKind) -> bool {
    matches!(
        kind,
        InputKind::Enum { .. }
            | InputKind::Int { .. }
            | InputKind::Float { .. }
            | InputKind::Bool { .. }
            | InputKind::Text { .. }
    )
}

/// Whether the frontend stores a phantom `control_after_generate` value after this input's
/// widget value (declared in the meta, or implied by seed naming on older cores).
pub(crate) fn takes_seed_control(input: &InputSchema) -> bool {
    match input.kind {
        InputKind::Int { control, .. } => {
            control || input.name == "seed" || input.name == "noise_seed"
        }
        _ => false,
    }
}

/// Iterates `widgets_values` (array form consumes in widget order and skips seed-control phantom
/// entries; dict form looks up by name).
enum WidgetValues<'a> {
    Arr { values: &'a [Value], cursor: usize },
    Dict(&'a serde_json::Map<String, Value>),
    None,
}

impl<'a> WidgetValues<'a> {
    fn new(v: Option<&'a Value>) -> Self {
        match v {
            Some(Value::Array(a)) => Self::Arr { values: a, cursor: 0 },
            Some(Value::Object(o)) => Self::Dict(o),
            _ => Self::None,
        }
    }

    fn next_for(&mut self, input: &InputSchema) -> (Option<&'a Value>, Option<&'a str>) {
        match self {
            Self::Arr { values, cursor } => {
                let v = match values.get(*cursor) {
                    Some(v) => v,
                    None => return (None, None),
                };
                *cursor += 1;
                let control = if takes_seed_control(input)
                    && values
                        .get(*cursor)
                        .and_then(Value::as_str)
                        .is_some_and(|s| SEED_CONTROLS.contains(&s))
                {
                    let s = values[*cursor].as_str();
                    *cursor += 1;
                    s
                } else {
                    None
                };
                (Some(v), control)
            }
            Self::Dict(map) => (map.get(&input.name), None),
            Self::None => (None, None),
        }
    }

    fn leftover(&self) -> Option<usize> {
        match self {
            Self::Arr { values, cursor } if *cursor < values.len() => Some(values.len() - cursor),
            _ => None,
        }
    }

    /// Dict-form entries the schema doesn't declare, minus frontend-only state. Empty for array
    /// form, whose trailing extras have no names to forward under.
    fn undeclared(&self, schema: &NodeSchema) -> Vec<(&'a String, &'a Value)> {
        let Self::Dict(map) = self else { return Vec::new() };
        map.iter()
            .filter(|(k, _)| {
                !UI_ONLY_WIDGETS.contains(&k.as_str())
                    && !schema.inputs.iter().any(|i| i.name == **k)
            })
            .collect()
    }
}

/// Coerce a widget JSON value into a [`WorkflowInput`] matching the schema kind.
fn coerce(kind: &InputKind, v: &Value) -> Option<WorkflowInput> {
    if v.is_null() {
        return None;
    }
    match kind {
        InputKind::Int { .. } => v
            .as_i64()
            .map(WorkflowInput::I64)
            .or_else(|| v.as_u64().map(WorkflowInput::U64))
            .or_else(|| v.as_f64().map(|f| WorkflowInput::I64(f.round() as i64)))
            .or_else(|| v.as_str()?.trim().parse().ok().map(WorkflowInput::I64)),
        InputKind::Float { .. } => v
            .as_f64()
            .map(WorkflowInput::F64)
            .or_else(|| v.as_str()?.trim().parse().ok().map(WorkflowInput::F64)),
        InputKind::Bool { .. } => v
            .as_bool()
            .map(WorkflowInput::Boolean)
            .or_else(|| v.as_str().map(|s| WorkflowInput::Boolean(s == "true"))),
        InputKind::Enum { .. } | InputKind::Text { .. } => Some(WorkflowInput::String(
            v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
        )),
        InputKind::Connection { .. } | InputKind::Opaque => None,
    }
}

/// Like [`coerce`], but a connection-kind target (a primitive feeding a socket) falls back to the
/// value's natural JSON type.
fn coerce_or_text(kind: &InputKind, v: &Value) -> Option<WorkflowInput> {
    coerce(kind, v).or_else(|| match v {
        Value::String(s) => Some(WorkflowInput::String(s.clone())),
        Value::Number(n) => n
            .as_i64()
            .map(WorkflowInput::I64)
            .or_else(|| n.as_f64().map(WorkflowInput::F64)),
        Value::Bool(b) => Some(WorkflowInput::Boolean(*b)),
        _ => None,
    })
}

/// The schema default for a widget input whose saved value is absent, so required widget inputs
/// still queue.
fn default_input(kind: &InputKind) -> Option<WorkflowInput> {
    match kind {
        InputKind::Enum { options, default, .. } => default
            .clone()
            .or_else(|| options.first().cloned())
            .map(WorkflowInput::String),
        InputKind::Int { default, .. } => Some(WorkflowInput::I64(*default)),
        InputKind::Float { default, .. } => Some(WorkflowInput::F64(*default)),
        InputKind::Bool { default } => Some(WorkflowInput::Boolean(*default)),
        InputKind::Text { default, .. } => Some(WorkflowInput::String(default.clone())),
        InputKind::Connection { .. } | InputKind::Opaque => None,
    }
}

// ── Subgraph flattening ───────────────────────────────────────────────────────

/// Where a subgraph output port draws from inside the definition.
enum SrcPort {
    /// An internal node's output.
    Node(i64, i64),
    /// Passed straight through from an input port.
    Input(i64),
}

/// A parsed link row in array or object form: `(id, from, from_slot, to, to_slot, type)`.
fn link_row(v: &Value) -> Option<(i64, i64, i64, i64, i64, String)> {
    match v {
        Value::Array(a) if a.len() >= 5 => Some((
            a[0].as_i64()?,
            a[1].as_i64()?,
            a[2].as_i64()?,
            a[3].as_i64()?,
            a[4].as_i64()?,
            a.get(5).and_then(Value::as_str).unwrap_or("*").to_string(),
        )),
        Value::Object(o) => Some((
            o.get("id")?.as_i64()?,
            o.get("origin_id")?.as_i64()?,
            o.get("origin_slot")?.as_i64()?,
            o.get("target_id")?.as_i64()?,
            o.get("target_slot")?.as_i64()?,
            o.get("type").and_then(Value::as_str).unwrap_or("*").to_string(),
        )),
        _ => None,
    }
}

fn set_origin(row: &mut Value, from: i64, slot: i64) {
    match row {
        Value::Array(a) if a.len() >= 3 => {
            a[1] = from.into();
            a[2] = slot.into();
        }
        Value::Object(o) => {
            o.insert("origin_id".into(), from.into());
            o.insert("origin_slot".into(), slot.into());
        }
        _ => {}
    }
}

/// Inline every `definitions.subgraphs` instance (recursively — expanded bodies may contain
/// further instances) so the converter only sees plain nodes. `None` when the workflow declares
/// no subgraphs.
fn flatten_subgraphs(ui: &Value, schemas: &SchemaSet, warnings: &mut Vec<String>) -> Option<Value> {
    let defs: BTreeMap<String, &Value> = ui
        .get("definitions")?
        .get("subgraphs")?
        .as_array()?
        .iter()
        .filter_map(|s| Some((s.get("id")?.as_str()?.to_string(), s)))
        .collect();
    if defs.is_empty() {
        return None;
    }
    let mut nodes: Vec<Value> = ui.get("nodes")?.as_array()?.clone();
    let mut links: Vec<Value> =
        ui.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut next_node =
        1 + nodes.iter().filter_map(|n| n.get("id")?.as_i64()).max().unwrap_or(0);
    let mut next_link = 1 + links.iter().filter_map(|r| Some(link_row(r)?.0)).max().unwrap_or(0);

    let mut budget = 128;
    loop {
        let Some(idx) = nodes.iter().position(|n| defs.contains_key(node_type(n))) else { break };
        if budget == 0 {
            warnings.push("subgraphs nested too deep; remaining instances skipped".into());
            break;
        }
        budget -= 1;
        let inst = nodes.swap_remove(idx);
        let def = defs[node_type(&inst)];
        expand_instance(
            &inst, def, &defs, &mut nodes, &mut links, &mut next_node, &mut next_link, schemas,
            warnings,
        );
    }

    let mut flat = ui.clone();
    let obj = flat.as_object_mut()?;
    obj.remove("definitions");
    obj.insert("nodes".into(), Value::Array(nodes));
    obj.insert("links".into(), Value::Array(links));
    Some(flat)
}

/// Splice one subgraph instance: append its internal nodes/links under fresh ids, feed input
/// ports from the instance's parent links, apply promoted widget values, and rewire parent links
/// that originate at the instance's output ports.
#[allow(clippy::too_many_arguments)]
fn expand_instance(
    inst: &Value,
    def: &Value,
    defs: &BTreeMap<String, &Value>,
    nodes: &mut Vec<Value>,
    links: &mut Vec<Value>,
    next_node: &mut i64,
    next_link: &mut i64,
    schemas: &SchemaSet,
    warnings: &mut Vec<String>,
) {
    let inst_id = inst.get("id").and_then(Value::as_i64).unwrap_or(-1);
    let name = def.get("name").and_then(Value::as_str).unwrap_or("subgraph");
    if node_mode(inst) == MODE_MUTE {
        warnings.push(format!("muted subgraph {inst_id} ({name}) dropped"));
        return;
    }

    let io_id = |key: &str, default: i64| {
        def.get(key).and_then(|n| n.get("id")).and_then(Value::as_i64).unwrap_or(default)
    };
    let (in_id, out_id) = (io_id("inputNode", -10), io_id("outputNode", -20));

    // Input port declarations, the parent link feeding each port, and that link's origin.
    let def_inputs: Vec<(&str, &str)> = def
        .get("inputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|p| {
            (
                p.get("name").and_then(Value::as_str).unwrap_or(""),
                p.get("type").and_then(Value::as_str).unwrap_or("*"),
            )
        })
        .collect();
    let inst_inputs = inst.get("inputs").and_then(Value::as_array);
    let port_feed: Vec<Option<i64>> = def_inputs
        .iter()
        .enumerate()
        .map(|(i, (pname, _))| {
            let entry = inst_inputs
                .into_iter()
                .flatten()
                .find(|e| e.get("name").and_then(Value::as_str) == Some(*pname))
                .or_else(|| inst_inputs.and_then(|a| a.get(i)));
            entry.and_then(|e| e.get("link")).and_then(Value::as_i64)
        })
        .collect();
    let feed_src: Vec<Option<(i64, i64)>> = port_feed
        .iter()
        .map(|pl| {
            pl.and_then(|pl| {
                links.iter().find_map(|r| {
                    let (id, o, os, ..) = link_row(r)?;
                    (id == pl).then_some((o, os))
                })
            })
        })
        .collect();

    // A bypassed instance splices each output port to the first matching-type input port.
    if node_mode(inst) == MODE_BYPASS {
        let resolved: BTreeMap<i64, Option<(i64, i64)>> = def
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(j, p)| {
                let oty = p.get("type").and_then(Value::as_str).unwrap_or("*");
                let port = def_inputs
                    .iter()
                    .position(|(_, ity)| oty == "*" || *ity == "*" || *ity == oty);
                (j as i64, port.and_then(|p| feed_src.get(p).copied().flatten()))
            })
            .collect();
        rewire_instance_outputs(links, inst_id, &resolved, name, warnings);
        return;
    }

    // Fresh ids for internal nodes.
    let def_nodes: Vec<&Value> =
        def.get("nodes").and_then(Value::as_array).into_iter().flatten().collect();
    let mut id_map: BTreeMap<i64, i64> = BTreeMap::new();
    for n in &def_nodes {
        if let Some(id) = n.get("id").and_then(Value::as_i64) {
            id_map.insert(id, *next_node);
            *next_node += 1;
        }
    }

    // Internal links: keep node-to-node rows under fresh ids; boundary rows become port feeds.
    let mut link_map: BTreeMap<i64, i64> = BTreeMap::new();
    let mut consumer_feed: BTreeMap<(i64, i64), Option<i64>> = BTreeMap::new();
    let mut output_src: BTreeMap<i64, SrcPort> = BTreeMap::new();
    for row in def.get("links").and_then(Value::as_array).into_iter().flatten() {
        let Some((lid, o, os, t, ts, ty)) = link_row(row) else { continue };
        if o == in_id {
            if t == out_id {
                output_src.entry(ts).or_insert(SrcPort::Input(os));
            } else {
                let feed = port_feed.get(os as usize).copied().flatten();
                consumer_feed.insert((t, ts), feed);
            }
        } else if t == out_id {
            output_src.entry(ts).or_insert(SrcPort::Node(o, os));
        } else {
            let (Some(&fo), Some(&ft)) = (id_map.get(&o), id_map.get(&t)) else { continue };
            let fresh = *next_link;
            *next_link += 1;
            link_map.insert(lid, fresh);
            links.push(serde_json::json!([fresh, fo, os, ft, ts, ty]));
        }
    }

    let overrides = proxy_overrides(inst, warnings);

    // Clone internal nodes under fresh ids with rewired input links.
    for n in def_nodes {
        let Some(old) = n.get("id").and_then(Value::as_i64) else { continue };
        let mut clone = n.clone();
        let Some(obj) = clone.as_object_mut() else { continue };
        obj.insert("id".into(), id_map[&old].into());
        if let Some(inputs) = obj.get_mut("inputs").and_then(Value::as_array_mut) {
            for (slot, entry) in inputs.iter_mut().enumerate() {
                let Some(e) = entry.as_object_mut() else { continue };
                let new_link = if let Some(feed) = consumer_feed.get(&(old, slot as i64)) {
                    feed.map(Value::from).unwrap_or(Value::Null)
                } else {
                    e.get("link")
                        .and_then(Value::as_i64)
                        .and_then(|l| link_map.get(&l))
                        .map(|&l| Value::from(l))
                        .unwrap_or(Value::Null)
                };
                e.insert("link".into(), new_link);
            }
        }
        if let Some(outputs) = obj.get_mut("outputs").and_then(Value::as_array_mut) {
            for entry in outputs.iter_mut() {
                if let Some(e) = entry.as_object_mut() {
                    e.insert("links".into(), Value::Null);
                }
            }
        }
        apply_overrides(obj, old, &overrides, defs, schemas, warnings);
        nodes.push(clone);
    }

    // Parent links drawing from the instance's output ports now draw from internal producers.
    let resolved: BTreeMap<i64, Option<(i64, i64)>> = output_src
        .iter()
        .map(|(&port, src)| {
            let target = match *src {
                SrcPort::Node(o, os) => id_map.get(&o).map(|&f| (f, os)),
                SrcPort::Input(p) => feed_src.get(p as usize).copied().flatten(),
            };
            (port, target)
        })
        .collect();
    rewire_instance_outputs(links, inst_id, &resolved, name, warnings);
}

/// Point every parent link that originates at `inst_id` at its resolved internal source, dropping
/// links whose port has no source.
fn rewire_instance_outputs(
    links: &mut Vec<Value>,
    inst_id: i64,
    resolved: &BTreeMap<i64, Option<(i64, i64)>>,
    name: &str,
    warnings: &mut Vec<String>,
) {
    links.retain_mut(|row| {
        let Some((_, o, os, ..)) = link_row(row) else { return true };
        if o != inst_id {
            return true;
        }
        match resolved.get(&os).copied().flatten() {
            Some((from, slot)) => {
                set_origin(row, from, slot);
                true
            }
            None => {
                warnings.push(format!("subgraph {name} (id {inst_id}): output {os} has no source"));
                false
            }
        }
    });
}

/// `(internal node id, widget name, value)` triples from the instance's promoted widgets
/// (`properties.proxyWidgets` paired with `widgets_values` by position).
fn proxy_overrides(inst: &Value, warnings: &mut Vec<String>) -> Vec<(i64, String, Value)> {
    let proxies = inst
        .get("properties")
        .and_then(|p| p.get("proxyWidgets"))
        .and_then(Value::as_array);
    let values = inst.get("widgets_values").and_then(Value::as_array);
    let (Some(proxies), Some(values)) = (proxies, values) else {
        if inst.get("widgets_values").is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty)) {
            let id = inst.get("id").and_then(Value::as_i64).unwrap_or(-1);
            warnings.push(format!("subgraph instance {id}: widget values without a proxy map"));
        }
        return Vec::new();
    };
    proxies
        .iter()
        .zip(values)
        .filter_map(|(p, v)| {
            let pair = p.as_array()?;
            let node = match pair.first()? {
                Value::String(s) => s.parse().ok()?,
                Value::Number(n) => n.as_i64()?,
                _ => return None,
            };
            let name = pair.get(1)?.as_str()?;
            (name != "control_after_generate").then(|| (node, name.to_string(), v.clone()))
        })
        .collect()
}

/// Write promoted widget values into one cloned internal node (`old_id` is its pre-remap id).
fn apply_overrides(
    node: &mut serde_json::Map<String, Value>,
    old_id: i64,
    overrides: &[(i64, String, Value)],
    defs: &BTreeMap<String, &Value>,
    schemas: &SchemaSet,
    warnings: &mut Vec<String>,
) {
    let mine: Vec<_> = overrides.iter().filter(|(id, ..)| *id == old_id).collect();
    if mine.is_empty() {
        return;
    }
    let ty = node.get("type").and_then(Value::as_str).unwrap_or("").to_string();

    // A nested instance keeps array form; positions come from its own proxy list.
    if defs.contains_key(&ty) {
        let hits: Vec<(usize, Value)> = {
            let idx_of = |name: &str| {
                node.get("properties")
                    .and_then(|p| p.get("proxyWidgets"))
                    .and_then(Value::as_array)?
                    .iter()
                    .position(|p| p.get(1).and_then(Value::as_str) == Some(name))
            };
            mine.iter().filter_map(|(_, name, v)| Some((idx_of(name)?, v.clone()))).collect()
        };
        let slot = node.entry("widgets_values").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(arr) = slot.as_array_mut() {
            for (i, v) in hits {
                while arr.len() <= i {
                    arr.push(Value::Null);
                }
                arr[i] = v;
            }
        }
        return;
    }

    let Some(schema) = schemas.nodes.get(&ty) else {
        warnings.push(format!("promoted widget on unknown node type `{ty}` dropped"));
        return;
    };
    // Regular node: re-key widgets_values by name so overrides can land regardless of position.
    let mut dict = match node.get("widgets_values") {
        Some(Value::Object(o)) => o.clone(),
        Some(Value::Array(a)) => array_to_dict(schema, a),
        _ => serde_json::Map::new(),
    };
    for (_, name, v) in mine {
        dict.insert(name.clone(), v.clone());
    }
    node.insert("widgets_values".into(), Value::Object(dict));
}

/// Re-key an array-form `widgets_values` by widget name (schema order, dropping seed-control
/// phantoms), mirroring [`WidgetValues`] consumption.
fn array_to_dict(schema: &NodeSchema, arr: &[Value]) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    let mut cursor = 0usize;
    for input in schema.inputs.iter().filter(|i| is_widget(&i.kind)) {
        let Some(v) = arr.get(cursor) else { break };
        map.insert(input.name.clone(), v.clone());
        cursor += 1;
        if takes_seed_control(input)
            && arr
                .get(cursor)
                .and_then(Value::as_str)
                .is_some_and(|s| SEED_CONTROLS.contains(&s))
        {
            cursor += 1;
        }
    }
    map
}

// ── API → UI synthesis ────────────────────────────────────────────────────────

/// Dependency depth of `id`: longest Slot chain feeding it. Cycle-tolerant (a revisit scores 0).
fn api_depth(
    id: WorkflowNodeId,
    wf: &Workflow,
    memo: &mut BTreeMap<WorkflowNodeId, u32>,
    visiting: &mut std::collections::HashSet<WorkflowNodeId>,
) -> u32 {
    if let Some(&d) = memo.get(&id) {
        return d;
    }
    if !visiting.insert(id) {
        return 0;
    }
    let d = wf
        .0
        .get(&id)
        .and_then(|n| {
            n.inputs
                .values()
                .filter_map(WorkflowInput::as_slot)
                .filter(|(src, _)| wf.0.contains_key(src))
                .map(|(src, _)| api_depth(src, wf, memo, visiting) + 1)
                .max()
        })
        .unwrap_or(0);
    visiting.remove(&id);
    memo.insert(id, d);
    d
}

/// The UI-format socket type string for a schema input kind.
fn ui_type(kind: &InputKind) -> String {
    match kind {
        InputKind::Connection { ty } => ty.clone(),
        InputKind::Enum { .. } => "COMBO".into(),
        InputKind::Int { .. } => "INT".into(),
        InputKind::Float { .. } => "FLOAT".into(),
        InputKind::Bool { .. } => "BOOLEAN".into(),
        InputKind::Text { .. } => "STRING".into(),
        InputKind::Opaque => "*".into(),
    }
}

/// A scalar [`WorkflowInput`] as the JSON value the UI file stores; enum display text maps back
/// through `typed` so numeric COMBOs stay numbers. `None` for slots.
fn scalar_json(input: &WorkflowInput, kind: Option<&InputKind>) -> Option<Value> {
    Some(match input {
        WorkflowInput::String(s) => kind
            .and_then(|k| k.enum_typed_value(s))
            .cloned()
            .unwrap_or_else(|| Value::String(s.clone())),
        WorkflowInput::I64(v) => serde_json::json!(v),
        WorkflowInput::U64(v) => serde_json::json!(v),
        WorkflowInput::F64(v) => serde_json::json!(v),
        WorkflowInput::Boolean(b) => Value::Bool(*b),
        WorkflowInput::Slot(..) => return None,
    })
}

/// The widget value a UI file stores for a schema input the API workflow leaves unset (or feeds
/// by link): the schema default.
fn default_json(kind: &InputKind) -> Value {
    match kind {
        InputKind::Enum { options, default, .. } => {
            let text = default.clone().or_else(|| options.first().cloned()).unwrap_or_default();
            let typed = kind.enum_typed_value(&text).cloned();
            typed.unwrap_or(Value::String(text))
        }
        InputKind::Int { default, .. } => serde_json::json!(default),
        InputKind::Float { default, .. } => serde_json::json!(default),
        InputKind::Bool { default } => Value::Bool(*default),
        InputKind::Text { default, .. } => Value::String(default.clone()),
        _ => Value::Null,
    }
}

/// Synthesize UI-format JSON (legacy 0.4 shape) from an API-format workflow, for embedding in the
/// PNG when no graph doc backs the job. Inputs and widget values follow schema order; nodes lay
/// out left-to-right by dependency depth. Inverse of [`convert`] up to layout.
pub fn api_to_ui(wf: &Workflow, schemas: &SchemaSet) -> Value {
    use serde_json::json;

    let mut memo = BTreeMap::new();
    let mut visiting = std::collections::HashSet::new();
    let mut order: Vec<WorkflowNodeId> = wf.0.keys().copied().collect();
    order.sort_by_key(|&id| (api_depth(id, wf, &mut memo, &mut visiting), id.0));

    // Ordered input names per node: schema inputs first (widget order), then undeclared extras.
    let ordered_inputs = |id: WorkflowNodeId| -> Vec<String> {
        let node = &wf.0[&id];
        let schema = schemas.nodes.get(&node.class_type);
        let mut names: Vec<String> =
            schema.map(|s| s.inputs.iter().map(|i| i.name.clone()).collect()).unwrap_or_default();
        let mut extras: Vec<&String> =
            node.inputs.keys().filter(|k| !names.iter().any(|n| &n == k)).collect();
        extras.sort();
        names.extend(extras.into_iter().cloned());
        names
    };

    let mut link_rows = Vec::new();
    let mut last_link = 0u64;
    let mut in_links: HashMap<(WorkflowNodeId, usize), u64> = HashMap::new();
    let mut out_links: HashMap<(WorkflowNodeId, u32), Vec<u64>> = HashMap::new();
    for &id in &order {
        for (i, name) in ordered_inputs(id).iter().enumerate() {
            let Some((src, slot)) = wf.0[&id].inputs.get(name).and_then(WorkflowInput::as_slot)
            else {
                continue;
            };
            if !wf.0.contains_key(&src) {
                continue;
            }
            last_link += 1;
            let ty = schemas
                .nodes
                .get(&wf.0[&src].class_type)
                .and_then(|s| s.outputs.get(slot as usize))
                .map(|o| o.ty.clone())
                .unwrap_or_else(|| "*".to_string());
            link_rows.push(json!([last_link, src.0, slot, id.0, i, ty]));
            in_links.insert((id, i), last_link);
            out_links.entry((src, slot)).or_default().push(last_link);
        }
    }

    let mut nodes = Vec::new();
    let mut row_in_depth: HashMap<u32, u32> = HashMap::new();
    for (pos, &id) in order.iter().enumerate() {
        let node = &wf.0[&id];
        let schema = schemas.nodes.get(&node.class_type);
        let depth = memo.get(&id).copied().unwrap_or(0);
        let row = row_in_depth.entry(depth).or_insert(0);

        let inputs: Vec<Value> = ordered_inputs(id)
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let kind = schema.and_then(|s| s.inputs.iter().find(|si| &si.name == name));
                let mut entry = json!({
                    "name": name,
                    "type": kind.map(|si| ui_type(&si.kind)).unwrap_or_else(|| "*".into()),
                    "link": in_links.get(&(id, i)),
                });
                if kind.is_some_and(|si| is_widget(&si.kind)) {
                    entry["widget"] = json!({ "name": name });
                }
                entry
            })
            .collect();

        let outputs: Vec<Value> = schema
            .map(|s| {
                s.outputs
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        json!({
                            "name": o.name,
                            "type": o.ty,
                            "links": out_links.get(&(id, i as u32)).cloned().unwrap_or_default(),
                            "slot_index": i,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut widgets_values = Vec::new();
        for si in schema.map(|s| s.inputs.as_slice()).unwrap_or_default() {
            if !is_widget(&si.kind) {
                continue;
            }
            let value = node
                .inputs
                .get(&si.name)
                .and_then(|v| scalar_json(v, Some(&si.kind)))
                .unwrap_or_else(|| default_json(&si.kind));
            widgets_values.push(value);
            if takes_seed_control(si) {
                widgets_values.push(json!("fixed"));
            }
        }

        nodes.push(json!({
            "id": id.0,
            "type": node.class_type,
            "pos": [40.0 + depth as f32 * 280.0, 40.0 + *row as f32 * 160.0],
            "size": [240.0, 120.0],
            "flags": {},
            "order": pos,
            "mode": 0,
            "inputs": inputs,
            "outputs": outputs,
            "properties": { "Node name for S&R": node.class_type },
            "widgets_values": widgets_values,
        }));
        *row += 1;
    }

    json!({
        "last_node_id": order.iter().map(|id| id.0).max().unwrap_or(0),
        "last_link_id": last_link,
        "nodes": nodes,
        "links": link_rows,
        "groups": [],
        "config": {},
        "extra": {},
        "version": 0.4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn schemas() -> SchemaSet {
        schema::parse(
            &serde_json::from_str(
                r#"{
            "KSampler": {"input": {"required": {
                "model": ["MODEL"],
                "seed": ["INT", {"default": 0, "min": 0, "max": 18446744073709551615}],
                "steps": ["INT", {"default": 20}],
                "cfg": ["FLOAT", {"default": 8.0}],
                "sampler_name": [["euler", "uni_pc"]],
                "scheduler": [["normal", "simple"]],
                "denoise": ["FLOAT", {"default": 1.0}]
            }}, "input_order": {"required": ["model", "seed", "steps", "cfg", "sampler_name", "scheduler", "denoise"]},
                "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
            "UNETLoader": {"input": {"required": {"unet_name": [["u.safetensors"]]}},
                "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]},
            "LoraLoaderModelOnly": {"input": {"required": {
                "model": ["MODEL"], "lora_name": [["l.safetensors"]], "strength_model": ["FLOAT", {"default": 1.0}]
            }}, "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}
        }"#,
            )
            .unwrap(),
        )
    }

    /// UNETLoader(1) → bypassed Lora(2) → Reroute(3) → KSampler(4); a Note(5); a primitive(6)
    /// feeding seed via a converted widget socket.
    #[test]
    fn converts_through_bypass_reroute_and_primitive() {
        let ui = serde_json::json!({
            "nodes": [
                {"id": 1, "type": "UNETLoader", "mode": 0,
                 "outputs": [{"name": "MODEL", "type": "MODEL", "links": [10]}],
                 "widgets_values": ["u.safetensors"]},
                {"id": 2, "type": "LoraLoaderModelOnly", "mode": 4,
                 "inputs": [{"name": "model", "type": "MODEL", "link": 10}],
                 "outputs": [{"name": "MODEL", "type": "MODEL", "links": [11]}],
                 "widgets_values": ["l.safetensors", 0.8]},
                {"id": 3, "type": "Reroute", "mode": 0,
                 "inputs": [{"name": "", "type": "*", "link": 11}],
                 "outputs": [{"name": "", "type": "MODEL", "links": [12]}]},
                {"id": 4, "type": "KSampler", "mode": 0,
                 "inputs": [
                    {"name": "model", "type": "MODEL", "link": 12},
                    {"name": "seed", "type": "INT", "link": 13, "widget": {"name": "seed"}}
                 ],
                 "widgets_values": [999, "randomize", 30, 6.5, "uni_pc", "simple", 1.0]},
                {"id": 5, "type": "Note", "mode": 0, "widgets_values": ["hi"]},
                {"id": 6, "type": "PrimitiveNode", "mode": 0,
                 "outputs": [{"name": "INT", "type": "INT", "links": [13]}],
                 "widgets_values": [123456, "fixed"]}
            ],
            "links": [
                [10, 1, 0, 2, 0, "MODEL"],
                [11, 2, 0, 3, 0, "*"],
                [12, 3, 0, 4, 0, "MODEL"],
                [13, 6, 0, 4, 1, "INT"]
            ]
        });
        let c = convert(&ui, &schemas()).unwrap();
        assert_eq!(c.workflow.0.len(), 2, "only UNETLoader and KSampler queue");
        let ks = &c.workflow.0[&WorkflowNodeId(4)];
        // model spliced through the bypassed lora and the reroute, straight to node 1.
        assert_eq!(ks.inputs["model"], WorkflowInput::Slot("1".into(), 0));
        // seed came from the primitive (overriding the widget value 999).
        assert_eq!(ks.inputs["seed"], WorkflowInput::I64(123456));
        // control_after_generate "randomize" was skipped, so steps/cfg landed correctly.
        assert_eq!(ks.inputs["steps"], WorkflowInput::I64(30));
        assert_eq!(ks.inputs["cfg"], WorkflowInput::F64(6.5));
        assert_eq!(ks.inputs["sampler_name"], WorkflowInput::String("uni_pc".into()));
        assert_eq!(ks.inputs["denoise"], WorkflowInput::F64(1.0));
    }

    /// UNETLoader(1) → SetNode(2, "M"); GetNode(3, "M") → KSampler(4): the invisible wire resolves
    /// to node 1.
    #[test]
    fn resolves_set_get_node_wires() {
        let ui = serde_json::json!({
            "nodes": [
                {"id": 1, "type": "UNETLoader", "mode": 0,
                 "outputs": [{"name": "MODEL", "type": "MODEL", "links": [10]}],
                 "widgets_values": ["u.safetensors"]},
                {"id": 2, "type": "SetNode", "mode": 0,
                 "inputs": [{"name": "MODEL", "type": "MODEL", "link": 10}],
                 "outputs": [{"name": "*", "type": "*", "links": null}],
                 "widgets_values": ["M"]},
                {"id": 3, "type": "GetNode", "mode": 0,
                 "outputs": [{"name": "MODEL", "type": "MODEL", "links": [11]}],
                 "widgets_values": ["M"]},
                {"id": 4, "type": "KSampler", "mode": 0,
                 "inputs": [{"name": "model", "type": "MODEL", "link": 11}],
                 "widgets_values": [1, "fixed", 20, 8.0, "euler", "normal", 1.0]}
            ],
            "links": [
                [10, 1, 0, 2, 0, "MODEL"],
                [11, 3, 0, 4, 0, "MODEL"]
            ]
        });
        let c = convert(&ui, &schemas()).unwrap();
        assert_eq!(c.workflow.0.len(), 2);
        let ks = &c.workflow.0[&WorkflowNodeId(4)];
        assert_eq!(ks.inputs["model"], WorkflowInput::Slot("1".into(), 0));
    }

    /// UNETLoader(1) → subgraph instance(2) wrapping a LoraLoaderModelOnly → KSampler(3), with
    /// `lora_name` promoted onto the instance. Flattening splices the internal node in between
    /// and applies the promoted value.
    #[test]
    fn flattens_subgraph_instances() {
        let ui = serde_json::json!({
            "nodes": [
                {"id": 1, "type": "UNETLoader", "mode": 0,
                 "outputs": [{"name": "MODEL", "type": "MODEL", "links": [1]}],
                 "widgets_values": ["u.safetensors"]},
                {"id": 2, "type": "aaaa-bbbb", "mode": 0,
                 "inputs": [{"name": "in", "type": "MODEL", "link": 1}],
                 "outputs": [{"name": "out", "type": "MODEL", "links": [2]}],
                 "widgets_values": ["proxied.safetensors"],
                 "properties": {"proxyWidgets": [["5", "lora_name"]]}},
                {"id": 3, "type": "KSampler", "mode": 0,
                 "inputs": [{"name": "model", "type": "MODEL", "link": 2}],
                 "widgets_values": [7, "fixed", 20, 8.0, "euler", "normal", 1.0]}
            ],
            "links": [
                [1, 1, 0, 2, 0, "MODEL"],
                [2, 2, 0, 3, 0, "MODEL"]
            ],
            "definitions": {"subgraphs": [{
                "id": "aaaa-bbbb", "name": "wrap",
                "inputNode": {"id": -10}, "outputNode": {"id": -20},
                "inputs": [{"name": "in", "type": "MODEL"}],
                "outputs": [{"name": "out", "type": "MODEL"}],
                "nodes": [
                    {"id": 5, "type": "LoraLoaderModelOnly", "mode": 0,
                     "inputs": [{"name": "model", "type": "MODEL", "link": 100}],
                     "outputs": [{"name": "MODEL", "type": "MODEL", "links": [101]}],
                     "widgets_values": ["l.safetensors", 0.8]}
                ],
                "links": [
                    {"id": 100, "origin_id": -10, "origin_slot": 0, "target_id": 5, "target_slot": 0, "type": "MODEL"},
                    {"id": 101, "origin_id": 5, "origin_slot": 0, "target_id": -20, "target_slot": 0, "type": "MODEL"}
                ]
            }]}
        });
        let c = convert(&ui, &schemas()).unwrap();
        assert_eq!(c.workflow.0.len(), 3);
        // The internal lora got the first free id (4) and sits between loader and sampler.
        let lora = &c.workflow.0[&WorkflowNodeId(4)];
        assert_eq!(lora.class_type, "LoraLoaderModelOnly");
        assert_eq!(lora.inputs["model"], WorkflowInput::Slot("1".into(), 0));
        assert_eq!(lora.inputs["lora_name"], WorkflowInput::String("proxied.safetensors".into()));
        assert_eq!(lora.inputs["strength_model"], WorkflowInput::F64(0.8));
        let ks = &c.workflow.0[&WorkflowNodeId(3)];
        assert_eq!(ks.inputs["model"], WorkflowInput::Slot("4".into(), 0));
    }

    #[test]
    fn dict_widgets_and_missing_values_fall_back() {
        let ui = serde_json::json!({
            "nodes": [{"id": 1, "type": "KSampler", "mode": 0,
                "widgets_values": {"steps": 12, "sampler_name": "uni_pc"}}],
            "links": []
        });
        let c = convert(&ui, &schemas()).unwrap();
        let ks = &c.workflow.0[&WorkflowNodeId(1)];
        assert_eq!(ks.inputs["steps"], WorkflowInput::I64(12));
        assert_eq!(ks.inputs["sampler_name"], WorkflowInput::String("uni_pc".into()));
        // Missing widget values fall back to schema defaults.
        assert_eq!(ks.inputs["cfg"], WorkflowInput::F64(8.0));
        assert_eq!(ks.inputs["scheduler"], WorkflowInput::String("normal".into()));
    }

    /// The VideoHelperSuite family stores widgets by name and adds format-dependent ones the
    /// server never declares. Dropping them re-encodes at the node's own defaults, silently
    /// changing quality and pixel format.
    #[test]
    fn undeclared_dict_widget_keys_are_forwarded() {
        let schemas = schema::parse(
            &serde_json::from_str(
                r#"{"VHS_VideoCombine": {"input": {"required": {
                    "images": ["IMAGE"],
                    "frame_rate": ["FLOAT", {"default": 8.0}],
                    "filename_prefix": ["STRING", {"default": "AnimateDiff"}],
                    "format": [["video/h264-mp4"]]
                }}, "output": [], "output_name": [], "output_is_list": []}}"#,
            )
            .unwrap(),
        );
        let ui = serde_json::json!({
            "nodes": [{"id": 1, "type": "VHS_VideoCombine", "mode": 0, "widgets_values": {
                "frame_rate": 32,
                "format": "video/h264-mp4",
                "crf": 19,
                "pix_fmt": "yuv420p",
                "save_metadata": true,
                "trim_to_audio": false,
                "control_after_generate": "randomize",
                "videopreview": {"hidden": false, "params": {"filename": "x.mp4"}}
            }}],
            "links": []
        });
        let n = &convert(&ui, &schemas).unwrap().workflow.0[&WorkflowNodeId(1)];
        // The four undeclared encoder settings survive with their own JSON types.
        assert_eq!(n.inputs["crf"], WorkflowInput::I64(19));
        assert_eq!(n.inputs["pix_fmt"], WorkflowInput::String("yuv420p".into()));
        assert_eq!(n.inputs["save_metadata"], WorkflowInput::Boolean(true));
        assert_eq!(n.inputs["trim_to_audio"], WorkflowInput::Boolean(false));
        // Frontend state is not an input.
        assert!(!n.inputs.contains_key("videopreview"));
        assert!(!n.inputs.contains_key("control_after_generate"));
        // Declared keys still come from the schema pass, not the forwarding one.
        assert_eq!(n.inputs["frame_rate"], WorkflowInput::F64(32.0));
        assert_eq!(n.inputs["filename_prefix"], WorkflowInput::String("AnimateDiff".into()));
    }

    /// The `RIFE VFI.scale_factor` chain end to end: whatever spelling the workflow file used, the
    /// queued prompt must carry the number the server's option list holds — never `options[0]`,
    /// and never a string.
    #[test]
    fn a_numeric_combo_survives_conversion_as_a_number() {
        let schemas = schema::parse(
            &serde_json::from_str(
                r#"{"RIFE VFI": {"input": {"required": {
                    "frames": ["IMAGE"],
                    "ckpt_name": [["rife49.pth"]],
                    "clear_cache_after_n_frames": ["INT", {"default": 10}],
                    "multiplier": ["INT", {"default": 2}],
                    "fast_mode": ["BOOLEAN", {"default": true}],
                    "ensemble": ["BOOLEAN", {"default": true}],
                    "scale_factor": [[0.25, 0.5, 1.0, 2.0, 4.0], {"default": 1.0}]
                }}, "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        );
        // The integer spelling is the one that used to become "0.25".
        for stored in [serde_json::json!(1), serde_json::json!(1.0), serde_json::json!("1.0")] {
            let ui = serde_json::json!({
                "nodes": [{"id": 25, "type": "RIFE VFI", "mode": 0,
                    "widgets_values": ["rife49.pth", 10, 2, false, true, stored]}],
                "links": []
            });
            let mut wf = convert(&ui, &schemas).unwrap().workflow;
            crate::preflight::retype_combo_values(&mut wf, &schemas);
            assert_eq!(
                wf.0[&WorkflowNodeId(25)].inputs["scale_factor"],
                WorkflowInput::F64(1.0),
                "stored as {stored}"
            );
        }
    }

    /// Real server workflows: set WORKFLOW_UI_JSON to a colon-separated list of UI-format files
    /// (requires OBJECT_INFO_JSON too).
    #[test]
    fn real_workflow_fixtures() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        for path in wf_paths.split(':') {
            let ui: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let c = convert(&ui, &schemas).unwrap_or_else(|e| panic!("{path}: {e}"));
            println!(
                "{path}: {} nodes, {} warnings",
                c.workflow.0.len(),
                c.warnings.len()
            );
            for w in &c.warnings {
                println!("  warn: {w}");
            }
            assert!(!c.workflow.0.is_empty());
            // Every slot reference must point at a node that exists in the converted workflow.
            for (id, node) in &c.workflow.0 {
                for (name, input) in &node.inputs {
                    if let WorkflowInput::Slot(target, _) = input {
                        let target: WorkflowNodeId = target.parse().unwrap();
                        assert!(
                            c.workflow.0.contains_key(&target),
                            "{path}: node {id} input {name} points at missing node {target}"
                        );
                    }
                }
            }
        }
    }

    /// UNETLoader(1) → Lora(2) → KSampler(3): the synthesized UI JSON reconverts to the same
    /// classes and inputs (links, widget values, seed phantom skipped).
    #[test]
    fn api_to_ui_round_trips_through_convert() {
        let mut wf = Workflow::default();
        wf.0.insert(
            WorkflowNodeId(1),
            WorkflowNode::new("UNETLoader").with_input("unet_name", "u.safetensors"),
        );
        wf.0.insert(
            WorkflowNodeId(2),
            WorkflowNode::new("LoraLoaderModelOnly")
                .with_input("model", WorkflowInput::slot(WorkflowNodeId(1), 0))
                .with_input("lora_name", "l.safetensors")
                .with_input("strength_model", 0.8f64),
        );
        wf.0.insert(
            WorkflowNodeId(3),
            WorkflowNode::new("KSampler")
                .with_input("model", WorkflowInput::slot(WorkflowNodeId(2), 0))
                .with_input("seed", 123i64)
                .with_input("steps", 30i64)
                .with_input("cfg", 6.5f64)
                .with_input("sampler_name", "uni_pc")
                .with_input("scheduler", "simple")
                .with_input("denoise", 1.0f64),
        );
        let s = schemas();
        let ui = api_to_ui(&wf, &s);
        assert!(ui["nodes"].as_array().is_some_and(|n| n.len() == 3));
        assert!(ui["links"].as_array().is_some_and(|l| l.len() == 2));
        let c = convert(&ui, &s).unwrap();
        assert_eq!(c.workflow.0.len(), wf.0.len());
        for (id, node) in &wf.0 {
            let got = &c.workflow.0[id];
            assert_eq!(got.class_type, node.class_type, "node {id}");
            assert_eq!(got.inputs, node.inputs, "node {id}");
        }
    }
}
