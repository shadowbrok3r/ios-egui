//! Apps: composite capabilities appended to the Create-tab graph as data instead of Rust.
//!
//! An [`AppDef`] is a graph fragment (node class names plus inputs), a set of promoted [`Knob`]s,
//! and a requirements list. [`apply`] walks the user's configured [`AppStep`]s in order, emits each
//! fragment onto the same [`WorkflowGraph`] the typed base graph used, and rebinds the running
//! IMAGE handle so steps chain. Adding an upscaler or a face fix is a JSON file, not a code change.

use std::collections::{BTreeMap, HashMap};

use rucomfyui::nodes::types::{
    ClipOut, ConditioningOut, ImageOut, LatentOut, ModelOut, Out, VaeOut,
};
use rucomfyui::workflow::{WorkflowInput, WorkflowNode, WorkflowNodeId};
use rucomfyui::{WorkflowGraph, workflow::WorkflowMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::{InputKind, SchemaSet};
use crate::types::{AppStep, Params};

fn one() -> u32 {
    1
}
fn one_i() -> i64 {
    1
}

// ── The authored artifact ────────────────────────────────────────────────────

/// A composite capability: a graph fragment plus its promoted knobs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppDef {
    /// Stable key stored in [`AppStep::app`]; a public wire format once shipped.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Picker section and default insert ordering ("Upscale", "Faces", "Finish").
    #[serde(default)]
    pub group: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub requires: Vec<Require>,
    #[serde(default)]
    pub knobs: Vec<Knob>,
    pub nodes: Vec<NodeTpl>,
    /// Local node and slot producing the IMAGE handed to the next step.
    pub output: LocalRef,
}

/// A node class the app needs, and the pack that provides it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Require {
    pub class: String,
    /// Shown verbatim in the "not installed" chip.
    pub pack: String,
    /// When missing, drop the nodes that name it in `needs` and keep running.
    #[serde(default)]
    pub optional: bool,
}

/// One node in the fragment. Input values are literals or `$`-prefixed references.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeTpl {
    /// Local id, unique within this app.
    pub id: String,
    pub class: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    /// Skip this node when the named optional requirement is unmet.
    #[serde(default)]
    pub needs: Option<String>,
}

/// A node id local to an [`AppDef`], with an output slot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LocalRef {
    pub node: String,
    #[serde(default)]
    pub slot: u32,
}

/// A parameter promoted out of the fragment into the Create-tab card.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Knob {
    pub id: String,
    pub label: String,
    pub ty: KnobTy,
    pub default: Value,
    /// Rendered behind the card's "More" collapsible.
    #[serde(default)]
    pub advanced: bool,
    #[serde(default)]
    pub tooltip: String,
}

/// Enum options are never stored; they resolve live from the connected server's catalog.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KnobTy {
    Enum {
        class: String,
        input: String,
        /// Keep only options starting with this prefix (`"bbox/"`).
        #[serde(default)]
        prefix: Option<String>,
    },
    Choice {
        options: Vec<String>,
    },
    Int {
        min: i64,
        max: i64,
        #[serde(default = "one_i")]
        step: i64,
    },
    Float {
        min: f64,
        max: f64,
        #[serde(default)]
        step: f64,
    },
    Bool,
    Text {
        #[serde(default)]
        multiline: bool,
    },
}

impl AppDef {
    pub fn knob(&self, id: &str) -> Option<&Knob> {
        self.knobs.iter().find(|k| k.id == id)
    }

    /// How the graph editor should materialize this app: widget literals plus intra-app links.
    /// Boundary references (`$image`, `$model`, `$param:…`) have no source outside a generation,
    /// so they are reported as open inputs for the user to wire rather than guessed at.
    pub fn plan(&self, step: Option<&AppStep>) -> Vec<PlannedNode> {
        let mut out = Vec::new();
        for tpl in &self.nodes {
            let mut planned = PlannedNode {
                local: tpl.id.clone(),
                class: tpl.class.clone(),
                optional: tpl.needs.clone(),
                literals: BTreeMap::new(),
                links: Vec::new(),
                open: Vec::new(),
            };
            for (name, raw) in &tpl.inputs {
                match as_ref(raw) {
                    None => {
                        planned
                            .literals
                            .insert(name.clone(), unescape(raw).unwrap_or_else(|| raw.clone()));
                    }
                    Some(Ok(Ref::Knob(id))) => {
                        let v = step
                            .and_then(|s| s.value(self, &id))
                            .or_else(|| self.knob(&id).map(|k| k.default.clone()));
                        if let Some(v) = v {
                            planned.literals.insert(name.clone(), v);
                        }
                    }
                    Some(Ok(Ref::Node(lr))) => {
                        planned.links.push((name.clone(), lr.node.clone(), lr.slot))
                    }
                    Some(Ok(r)) => planned.open.push((name.clone(), r)),
                    Some(Err(_)) => {}
                }
            }
            out.push(planned);
        }
        out
    }

    /// Structural problems that make the app unusable regardless of the server.
    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("app has no id".into());
        }
        if self.nodes.is_empty() {
            return Err("app has no nodes".into());
        }
        let mut seen: Vec<&str> = Vec::new();
        for n in &self.nodes {
            if seen.contains(&n.id.as_str()) {
                return Err(format!("duplicate node id '{}'", n.id));
            }
            seen.push(&n.id);
        }
        if !seen.contains(&self.output.node.as_str()) {
            return Err(format!("output names unknown node '{}'", self.output.node));
        }
        // Refs may only point backwards, which is what keeps the emitted graph acyclic.
        let mut defined: Vec<&str> = Vec::new();
        for n in &self.nodes {
            for (name, v) in &n.inputs {
                let where_ = || format!("node '{}' input '{name}'", n.id);
                match as_ref(v) {
                    Some(Err(e)) => return Err(format!("{}: {e}", where_())),
                    Some(Ok(Ref::Node(r))) if !defined.contains(&r.node.as_str()) => {
                        return Err(format!("{} references '{}' before it is declared", where_(), r.node));
                    }
                    Some(Ok(Ref::Knob(id))) if self.knob(&id).is_none() => {
                        return Err(format!("{} references undeclared knob '{id}'", where_()));
                    }
                    _ => {}
                }
            }
            defined.push(&n.id);
        }
        for k in &self.knobs {
            let bad = match &k.ty {
                KnobTy::Int { min, max, .. } => min > max,
                KnobTy::Float { min, max, .. } => min > max,
                KnobTy::Choice { options } => options.is_empty(),
                _ => false,
            };
            if bad {
                return Err(format!("knob '{}' has an empty or inverted range", k.id));
            }
        }
        Ok(())
    }
}

/// One node as the graph editor should create it.
pub struct PlannedNode {
    pub local: String,
    pub class: String,
    /// The optional requirement gating this node, if any.
    pub optional: Option<String>,
    /// Widget values to set, keyed by input name — never by index, since the editor re-sorts
    /// a node's inputs when it builds it.
    pub literals: BTreeMap<String, Value>,
    /// (input name, source local id, source output slot).
    pub links: Vec<(String, String, u32)>,
    /// Inputs the app expects from the surrounding graph.
    pub open: Vec<(String, Ref)>,
}

impl Ref {
    /// Short label for an unwired boundary input, e.g. "image" or "param:steps".
    pub fn label(&self) -> String {
        match self {
            Ref::Image => "image".into(),
            Ref::Latent => "latent".into(),
            Ref::Model => "model".into(),
            Ref::Clip => "clip".into(),
            Ref::Vae => "vae".into(),
            Ref::Positive => "positive".into(),
            Ref::Negative => "negative".into(),
            Ref::Seed(0) => "seed".into(),
            Ref::Seed(n) => format!("seed+{n}"),
            Ref::Param(f) => format!("param:{f:?}").to_lowercase(),
            Ref::Knob(k) => format!("knob:{k}"),
            Ref::Node(n) => format!("node:{}", n.node),
        }
    }
}

// ── References ───────────────────────────────────────────────────────────────

/// A `$`-prefixed input reference resolved at build time.
#[derive(Clone, Debug, PartialEq)]
pub enum Ref {
    Image,
    Latent,
    Model,
    Clip,
    Vae,
    Positive,
    Negative,
    /// `$seed` or `$seed+N`, wrapping.
    Seed(u64),
    Param(ParamField),
    Knob(String),
    Node(LocalRef),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamField {
    Steps,
    Cfg,
    Sampler,
    Scheduler,
    Width,
    Height,
    Denoise,
    Positive,
    Negative,
    Checkpoint,
    BatchSize,
}

/// `Some(Ok)` for a reference, `Some(Err)` for a malformed one, `None` for a literal.
fn as_ref(v: &Value) -> Option<Result<Ref, String>> {
    let s = v.as_str()?;
    let body = s.strip_prefix('$')?;
    // "$$name" escapes a literal "$name".
    if body.starts_with('$') {
        return None;
    }
    Some(Ref::parse(body))
}

/// A literal string with its `$$` escape removed, if it had one.
fn unescape(v: &Value) -> Option<Value> {
    let s = v.as_str()?;
    s.strip_prefix("$$").map(|rest| Value::String(format!("${rest}")))
}

impl Ref {
    /// Parse the text after the leading `$`.
    fn parse(body: &str) -> Result<Ref, String> {
        let (head, rest) = match body.split_once(':') {
            Some((h, r)) => (h, Some(r)),
            None => (body, None),
        };
        match (head, rest) {
            ("image", None) => Ok(Ref::Image),
            ("latent", None) => Ok(Ref::Latent),
            ("model", None) => Ok(Ref::Model),
            ("clip", None) => Ok(Ref::Clip),
            ("vae", None) => Ok(Ref::Vae),
            ("positive", None) => Ok(Ref::Positive),
            ("negative", None) => Ok(Ref::Negative),
            ("knob", Some(id)) if !id.is_empty() => Ok(Ref::Knob(id.to_string())),
            ("node", Some(r)) => {
                let (node, slot) = match r.split_once(':') {
                    Some((n, s)) => (
                        n,
                        s.parse::<u32>().map_err(|_| format!("bad slot index '{s}'"))?,
                    ),
                    None => (r, 0),
                };
                if node.is_empty() {
                    return Err("$node: needs a node id".into());
                }
                Ok(Ref::Node(LocalRef { node: node.to_string(), slot }))
            }
            ("param", Some(f)) => Ok(Ref::Param(match f {
                "steps" => ParamField::Steps,
                "cfg" => ParamField::Cfg,
                "sampler" => ParamField::Sampler,
                "scheduler" => ParamField::Scheduler,
                "width" => ParamField::Width,
                "height" => ParamField::Height,
                "denoise" => ParamField::Denoise,
                "positive" => ParamField::Positive,
                "negative" => ParamField::Negative,
                "checkpoint" => ParamField::Checkpoint,
                "batch_size" => ParamField::BatchSize,
                other => return Err(format!("unknown $param field '{other}'")),
            })),
            _ => {
                // "$seed" / "$seed+3": the only form carrying an arithmetic suffix.
                if let Some(off) = body.strip_prefix("seed") {
                    return match off {
                        "" => Ok(Ref::Seed(0)),
                        _ => off
                            .strip_prefix('+')
                            .and_then(|n| n.parse::<u64>().ok())
                            .map(Ref::Seed)
                            .ok_or_else(|| format!("bad seed offset '{off}'")),
                    };
                }
                Err(format!("unknown reference '${body}'"))
            }
        }
    }
}

// ── Build-time context ───────────────────────────────────────────────────────

/// Handles published by the base graph. `image` is rebound as each step runs.
pub struct Ctx {
    pub image: ImageOut,
    pub latent: LatentOut,
    pub model: ModelOut,
    pub clip: ClipOut,
    pub vae: VaeOut,
    pub positive: ConditioningOut,
    pub negative: ConditioningOut,
}

/// Emits only the inputs the connected server declares for `class`, recording what it dropped.
/// A FaceDetailer build that renamed or removed an input still queues with its own defaults.
pub struct NodeBuilder<'a> {
    set: &'a SchemaSet,
    class: String,
    inputs: HashMap<String, WorkflowInput>,
    dropped: Vec<String>,
    /// (input, wanted, used) where a COMBO value was replaced by an installed one.
    substituted: Vec<(String, String, String)>,
}

impl<'a> NodeBuilder<'a> {
    pub fn new(set: &'a SchemaSet, class: &str) -> Self {
        Self {
            set,
            class: class.to_string(),
            inputs: HashMap::new(),
            dropped: Vec::new(),
            substituted: Vec::new(),
        }
    }

    /// Coerce `v` to the variant this server's schema declares. Ints must not serialize as `20.0`.
    fn set_value(&mut self, name: &str, v: &Value) {
        let Some(kind) = self.set.input(&self.class, name).map(|i| &i.kind) else {
            self.dropped.push(name.to_string());
            return;
        };
        // A COMBO value this server does not offer fails /prompt validation for the WHOLE graph,
        // which would lose the base image too. Substitute an installed option and report it.
        if let InputKind::Enum { options, .. } = kind
            && !options.is_empty()
            && let Some(want) = v.as_str()
            && !options.iter().any(|o| o == want)
        {
            let used = options[0].clone();
            self.substituted.push((name.to_string(), want.to_string(), used.clone()));
            self.inputs.insert(name.to_string(), WorkflowInput::String(used));
            return;
        }
        if let Some(w) = coerce(v, kind) {
            self.inputs.insert(name.to_string(), w);
        } else {
            self.dropped.push(name.to_string());
        }
    }

    /// Wire an upstream output, skipping inputs this server does not declare.
    fn set_input(&mut self, name: &str, w: WorkflowInput) {
        if self.set.input(&self.class, name).is_none() {
            self.dropped.push(name.to_string());
            return;
        }
        self.inputs.insert(name.to_string(), w);
    }

    fn add(self, g: &WorkflowGraph) -> (WorkflowNodeId, Vec<String>, Vec<(String, String, String)>) {
        let id = g.add_dynamic(WorkflowNode {
            inputs: self.inputs,
            class_type: self.class.clone(),
            meta: Some(WorkflowMeta::new(self.class)),
        });
        (id, self.dropped, self.substituted)
    }
}

/// Map a JSON value onto the `WorkflowInput` variant matching the declared input kind.
fn coerce(v: &Value, kind: &InputKind) -> Option<WorkflowInput> {
    Some(match kind {
        InputKind::Int { .. } => match v {
            Value::Number(n) if n.is_u64() && n.as_i64().is_none() => {
                WorkflowInput::U64(n.as_u64()?)
            }
            Value::Number(n) => WorkflowInput::I64(n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?),
            Value::Bool(b) => WorkflowInput::I64(*b as i64),
            Value::String(s) => WorkflowInput::I64(s.parse().ok()?),
            _ => return None,
        },
        InputKind::Float { .. } => match v {
            Value::Number(n) => WorkflowInput::F64(n.as_f64()?),
            Value::String(s) => WorkflowInput::F64(s.parse().ok()?),
            _ => return None,
        },
        InputKind::Bool { .. } => match v {
            Value::Bool(b) => WorkflowInput::Boolean(*b),
            Value::Number(n) => WorkflowInput::Boolean(n.as_f64()? != 0.0),
            Value::String(s) => WorkflowInput::Boolean(s == "true" || s == "True"),
            _ => return None,
        },
        InputKind::Enum { .. } | InputKind::Text { .. } => match v {
            Value::String(s) => WorkflowInput::String(s.clone()),
            Value::Number(n) => WorkflowInput::String(n.to_string()),
            Value::Bool(b) => WorkflowInput::String(b.to_string()),
            _ => return None,
        },
        // A literal on a socket input, or an unrecognized spec: fall back to the JSON type.
        InputKind::Connection { .. } | InputKind::Opaque => match v {
            Value::String(s) => WorkflowInput::String(s.clone()),
            Value::Bool(b) => WorkflowInput::Boolean(*b),
            Value::Number(n) if n.is_i64() => WorkflowInput::I64(n.as_i64()?),
            Value::Number(n) if n.is_u64() => WorkflowInput::U64(n.as_u64()?),
            Value::Number(n) => WorkflowInput::F64(n.as_f64()?),
            _ => return None,
        },
    })
}

// ── Availability ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Ready,
    /// Runnable; a named model is absent, so the build will substitute an installed one.
    Degraded(Vec<String>),
    /// Class present but a knob's target input is gone: hide that control, drop the target.
    Mismatch(Vec<String>),
    /// Required class absent from the catalog.
    Missing(Vec<Require>),
    /// Required class present in `/object_info` but its schema failed to parse.
    Broken(Vec<(String, String)>),
    /// Not connected, so nothing can be checked yet.
    NoCatalog,
}

impl Status {
    pub fn runnable(&self) -> bool {
        !matches!(self, Status::Missing(_) | Status::Broken(_))
    }

    /// Short chip text for the picker and card headers.
    pub fn chip(&self) -> String {
        match self {
            Status::Ready => String::new(),
            Status::Degraded(w) => format!("check: {}", w.join(", ")),
            Status::Mismatch(w) => format!("{} option(s) unsupported", w.len()),
            Status::Missing(r) => {
                let packs: Vec<&str> = r.iter().map(|x| x.pack.as_str()).collect();
                format!("needs {}", packs.join(", "))
            }
            Status::Broken(b) => format!("{} schema failed to parse", b[0].0),
            Status::NoCatalog => "connect to check".into(),
        }
    }
}

/// Pre-flight availability of `def` against the connected catalog. No network.
pub fn status(def: &AppDef, step: Option<&AppStep>, schemas: Option<&SchemaSet>) -> Status {
    let Some(set) = schemas else { return Status::NoCatalog };

    let mut missing = Vec::new();
    let mut broken = Vec::new();
    for r in &def.requires {
        if r.optional || set.has_node(&r.class) {
            continue;
        }
        match set.skipped.iter().find(|(n, _)| *n == r.class) {
            Some((n, why)) => broken.push((n.clone(), why.clone())),
            None => missing.push(r.clone()),
        }
    }
    // A class the fragment emits but never declared still has to exist.
    for n in &def.nodes {
        if n.needs.is_some() || set.has_node(&n.class) {
            continue;
        }
        // Only a NON-optional require covers this: an optional one would let a missing class
        // pass as Ready and then get dropped mid-chain, leaving downstream sockets unset.
        if def.requires.iter().any(|r| r.class == n.class && !r.optional) {
            continue;
        }
        match set.skipped.iter().find(|(s, _)| *s == n.class) {
            Some((s, why)) => broken.push((s.clone(), why.clone())),
            None => missing.push(Require {
                class: n.class.clone(),
                pack: n.class.clone(),
                optional: false,
            }),
        }
    }
    if !broken.is_empty() {
        return Status::Broken(broken);
    }
    if !missing.is_empty() {
        return Status::Missing(missing);
    }

    let mut mismatch = Vec::new();
    let mut degraded = Vec::new();
    for k in &def.knobs {
        // A knob whose target input vanished can no longer be rendered or sent.
        if let Some(target) = knob_target(def, &k.id)
            && set.input(&target.0, &target.1).is_none()
        {
            mismatch.push(k.label.clone());
            continue;
        }
        if let KnobTy::Enum { class, input, prefix } = &k.ty {
            let opts = enum_options(set, class, input, prefix.as_deref());
            let current = step
                .and_then(|s| s.values.get(&k.id))
                .unwrap_or(&k.default)
                .as_str()
                .unwrap_or_default()
                .to_string();
            if opts.is_empty() {
                degraded.push(format!("{}: none installed", k.label));
            } else if !current.is_empty() && !opts.contains(&current) {
                degraded.push(format!("{}: '{current}' not installed", k.label));
            }
        }
    }
    if !mismatch.is_empty() {
        return Status::Mismatch(mismatch);
    }
    if !degraded.is_empty() {
        return Status::Degraded(degraded);
    }
    Status::Ready
}

/// The (class, input) a knob feeds, found by scanning the fragment for its `$knob:` reference.
fn knob_target(def: &AppDef, knob: &str) -> Option<(String, String)> {
    for n in &def.nodes {
        if n.needs.is_some() {
            continue;
        }
        for (name, v) in &n.inputs {
            if let Some(Ok(Ref::Knob(id))) = as_ref(v)
                && id == knob
            {
                return Some((n.class.clone(), name.clone()));
            }
        }
    }
    None
}

/// Enum options for a knob, filtered by its prefix when it has one.
pub fn enum_options(
    set: &SchemaSet,
    class: &str,
    input: &str,
    prefix: Option<&str>,
) -> Vec<String> {
    let all = set.enum_options(class, input);
    let Some(p) = prefix else { return all };
    let filtered: Vec<String> = all.iter().filter(|o| o.starts_with(p)).cloned().collect();
    if filtered.is_empty() { all } else { filtered }
}

// ── Composition ──────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Report {
    pub applied: Vec<String>,
    /// (app name, reason) for steps that could not run.
    pub skipped: Vec<(String, String)>,
    /// (class, input) pairs this server did not declare.
    pub dropped: Vec<(String, String)>,
    /// (class.input, wanted, used) where an uninstalled option was replaced.
    pub substituted: Vec<(String, String, String)>,
    /// Chain-ordering problems that still produce a valid prompt.
    pub warnings: Vec<String>,
}

impl Report {
    /// One user-facing line, empty when everything applied cleanly.
    pub fn note(&self) -> String {
        let mut parts = Vec::new();
        if !self.skipped.is_empty() {
            let names: Vec<String> =
                self.skipped.iter().map(|(n, w)| format!("{n} ({w})")).collect();
            parts.push(format!("Skipped {}: {}", self.skipped.len(), names.join("; ")));
        }
        if !self.substituted.is_empty() {
            let each: Vec<String> = self
                .substituted
                .iter()
                .map(|(at, want, used)| format!("{at}: '{want}' not installed, used '{used}'"))
                .collect();
            parts.push(each.join("; "));
        }
        parts.extend(self.warnings.iter().cloned());
        if !self.dropped.is_empty() {
            let mut by_class: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (c, i) in &self.dropped {
                by_class.entry(c).or_default().push(i);
            }
            let each: Vec<String> = by_class
                .iter()
                .map(|(c, ins)| format!("{c}: {}", ins.join(", ")))
                .collect();
            parts.push(format!("Inputs not supported by this build — {}", each.join("; ")));
        }
        parts.join(" · ")
    }
}

/// Append each enabled step's fragment to `g`, rebinding `ctx.image` as the chain advances.
/// Unrunnable steps are skipped and recorded rather than failing the generation.
pub fn apply(
    g: &WorkflowGraph,
    ctx: &mut Ctx,
    steps: &[AppStep],
    set: &AppSet,
    schemas: &SchemaSet,
    p: &Params,
) -> Report {
    let mut report = Report::default();
    let mut image_rebound = false;
    for step in steps.iter().filter(|s| s.enabled) {
        let Some(def) = set.by_id.get(&step.app) else {
            report.skipped.push((step.app.clone(), "app not installed".into()));
            continue;
        };
        let st = status(def, Some(step), Some(schemas));
        if !st.runnable() {
            report.skipped.push((def.name.clone(), st.chip()));
            continue;
        }
        // A step that re-renders from the base latent ignores whatever the image chain has
        // built so far, so anything above it is wasted work. Say so rather than silently
        // discarding it.
        if image_rebound && reads_latent(def) {
            report.warnings.push(format!(
                "{} re-renders from the base image — move it above the other steps",
                def.name
            ));
        }
        match apply_one(g, ctx, def, step, schemas, p) {
            Ok(out) => {
                report.applied.push(def.name.clone());
                report.dropped.extend(out.dropped);
                report.substituted.extend(out.substituted);
                image_rebound = true;
            }
            Err(e) => report.skipped.push((def.name.clone(), e)),
        }
    }
    report
}

/// Whether the fragment consumes the base latent instead of the running image.
fn reads_latent(def: &AppDef) -> bool {
    def.nodes.iter().any(|n| {
        n.inputs.values().any(|v| matches!(as_ref(v), Some(Ok(Ref::Latent))))
    })
}

#[derive(Default)]
struct Applied {
    dropped: Vec<(String, String)>,
    substituted: Vec<(String, String, String)>,
}

fn apply_one(
    g: &WorkflowGraph,
    ctx: &mut Ctx,
    def: &AppDef,
    step: &AppStep,
    schemas: &SchemaSet,
    p: &Params,
) -> Result<Applied, String> {
    let mut local: BTreeMap<String, WorkflowNodeId> = BTreeMap::new();
    let mut out = Applied::default();

    for tpl in &def.nodes {
        // An unmet optional requirement drops its nodes; refs to them drop with it.
        if let Some(req) = &tpl.needs
            && !schemas.has_node(req)
        {
            continue;
        }
        // An UNGATED node whose class is absent would leave downstream sockets unset and get the
        // whole prompt rejected, so fail the step here instead of emitting a broken fragment.
        if !schemas.has_node(&tpl.class) {
            return Err(format!("{} is not installed on this server", tpl.class));
        }
        let mut nb = NodeBuilder::new(schemas, &tpl.class);
        for (name, raw) in &tpl.inputs {
            match as_ref(raw) {
                Some(Ok(r)) => {
                    let mut subs = Vec::new();
                    let got = resolve(&r, ctx, def, step, p, schemas, &local, &mut subs);
                    out.substituted.extend(
                        subs.into_iter()
                            .map(|(w, u)| (format!("{}.{name}", tpl.class), w, u)),
                    );
                    match got {
                        Some(Resolved::Input(w)) => nb.set_input(name, w),
                        Some(Resolved::Value(v)) => nb.set_value(name, &v),
                        // A reference into a skipped optional node: leave the input unset.
                        None => {}
                    }
                }
                Some(Err(e)) => return Err(format!("node '{}' input '{name}': {e}", tpl.id)),
                None => nb.set_value(name, &unescape(raw).unwrap_or_else(|| raw.clone())),
            }
        }
        let (id, drop, subs) = nb.add(g);
        out.dropped.extend(drop.into_iter().map(|i| (tpl.class.clone(), i)));
        out.substituted.extend(
            subs.into_iter().map(|(i, w, u)| (format!("{}.{i}", tpl.class), w, u)),
        );
        local.insert(tpl.id.clone(), id);
    }

    let node = local
        .get(&def.output.node)
        .ok_or_else(|| format!("output node '{}' was not emitted", def.output.node))?;
    ctx.image = ImageOut::from_dynamic(*node, def.output.slot);
    Ok(out)
}

enum Resolved {
    /// An upstream slot or an already-typed scalar.
    Input(WorkflowInput),
    /// A JSON value still needing schema-directed coercion.
    Value(Value),
}

/// A knob's effective value, with an enum knob naming an uninstalled option replaced by an
/// installed one that still honours the knob's prefix. Records what it swapped.
fn knob_value(
    def: &AppDef,
    step: &AppStep,
    schemas: &SchemaSet,
    id: &str,
    subs: &mut Vec<(String, String)>,
) -> Option<Value> {
    let v = step.value(def, id)?;
    let Some(Knob { ty: KnobTy::Enum { class, input, prefix }, .. }) = def.knob(id) else {
        return Some(v);
    };
    let want = v.as_str().unwrap_or_default();
    let opts = enum_options(schemas, class, input, prefix.as_deref());
    if opts.iter().any(|o| o == want) {
        return Some(v);
    }
    // Empty options means the server does not describe this as a COMBO; send it verbatim.
    match opts.first() {
        Some(used) => {
            subs.push((want.to_string(), used.clone()));
            Some(Value::from(used.clone()))
        }
        None => Some(v),
    }
}

fn resolve(
    r: &Ref,
    ctx: &Ctx,
    def: &AppDef,
    step: &AppStep,
    p: &Params,
    schemas: &SchemaSet,
    local: &BTreeMap<String, WorkflowNodeId>,
    subs: &mut Vec<(String, String)>,
) -> Option<Resolved> {
    Some(match r {
        Ref::Image => Resolved::Input(ctx.image.into_input()),
        Ref::Latent => Resolved::Input(ctx.latent.into_input()),
        Ref::Model => Resolved::Input(ctx.model.into_input()),
        Ref::Clip => Resolved::Input(ctx.clip.into_input()),
        Ref::Vae => Resolved::Input(ctx.vae.into_input()),
        Ref::Positive => Resolved::Input(ctx.positive.into_input()),
        Ref::Negative => Resolved::Input(ctx.negative.into_input()),
        Ref::Seed(off) => Resolved::Value(Value::from(p.seed.wrapping_add(*off))),
        Ref::Param(f) => Resolved::Value(match f {
            ParamField::Steps => Value::from(p.steps),
            ParamField::Cfg => Value::from(p.cfg),
            ParamField::Sampler => Value::from(p.sampler.clone()),
            ParamField::Scheduler => Value::from(p.scheduler.clone()),
            ParamField::Width => Value::from(p.width),
            ParamField::Height => Value::from(p.height),
            ParamField::Denoise => Value::from(p.denoise),
            ParamField::Positive => Value::from(p.combined_positive()),
            ParamField::Negative => Value::from(p.negative.clone()),
            ParamField::Checkpoint => Value::from(p.checkpoint.clone()),
            ParamField::BatchSize => Value::from(p.batch_size),
        }),
        // Substitute here rather than leaving it to NodeBuilder so the knob's prefix filter
        // applies — a face detector must fall back to another bbox/ model, not a segm/ one.
        Ref::Knob(id) => Resolved::Value(knob_value(def, step, schemas, id, subs)?),
        Ref::Node(lr) => Resolved::Input(WorkflowInput::slot(*local.get(&lr.node)?, lr.slot)),
    })
}

// ── The installed set ────────────────────────────────────────────────────────

/// Builtins compiled in, plus user apps from `{documents_dir}/comfyui/apps/*.json`.
#[derive(Default)]
pub struct AppSet {
    pub by_id: BTreeMap<String, AppDef>,
    /// (source, reason) for definitions that failed to load.
    pub bad: Vec<(String, String)>,
}

/// Shipped apps. All but `face.detailer` need only stock ComfyUI nodes.
const BUILTIN: &[(&str, &str)] = &[
    ("hires_fix.json", include_str!("apps_builtin/hires_fix.json")),
    ("face_detailer.json", include_str!("apps_builtin/face_detailer.json")),
    ("upscale_model.json", include_str!("apps_builtin/upscale_model.json")),
    ("upscale_scale.json", include_str!("apps_builtin/upscale_scale.json")),
    ("sharpen.json", include_str!("apps_builtin/sharpen.json")),
];

/// Pipeline order for groups; a new step inserts above the first step of a later group.
/// Hi-res re-renders from the base latent, so it has to precede anything image-based.
pub const GROUP_ORDER: &[&str] = &["Hi-res", "Faces", "Upscale", "Finish"];

pub fn group_rank(group: &str) -> usize {
    GROUP_ORDER.iter().position(|g| *g == group).unwrap_or(GROUP_ORDER.len())
}

impl AppSet {
    pub fn builtin() -> Self {
        let mut set = Self::default();
        for (name, body) in BUILTIN {
            set.insert_json(name, body);
        }
        set
    }

    /// Builtins plus any user apps found under `dir` (the host documents directory).
    pub fn load(dir: Option<&str>) -> Self {
        let mut set = Self::builtin();
        let Some(dir) = dir else { return set };
        let path = format!("{dir}/comfyui/apps");
        let Ok(entries) = std::fs::read_dir(&path) else { return set };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("?").to_string();
            match std::fs::read_to_string(&p) {
                Ok(body) => set.insert_json(&name, &body),
                Err(e) => set.bad.push((name, e.to_string())),
            }
        }
        set
    }

    /// Parse and validate one definition, replacing any builtin with the same id.
    pub fn insert_json(&mut self, source: &str, body: &str) {
        match serde_json::from_str::<AppDef>(body) {
            Ok(def) => match def.validate() {
                Ok(()) => {
                    self.by_id.insert(def.id.clone(), def);
                }
                Err(e) => self.bad.push((source.to_string(), e)),
            },
            Err(e) => self.bad.push((source.to_string(), e.to_string())),
        }
    }

    pub fn get(&self, id: &str) -> Option<&AppDef> {
        self.by_id.get(id)
    }

    /// Apps grouped for the picker: group name, then its apps in name order.
    pub fn grouped(&self) -> Vec<(String, Vec<&AppDef>)> {
        let mut by_group: BTreeMap<String, Vec<&AppDef>> = BTreeMap::new();
        for def in self.by_id.values() {
            let g = if def.group.is_empty() { "Other".to_string() } else { def.group.clone() };
            by_group.entry(g).or_default().push(def);
        }
        for v in by_group.values_mut() {
            v.sort_by(|a, b| a.name.cmp(&b.name));
        }
        by_group.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;

    fn schemas() -> SchemaSet {
        crate::schema::parse(
            &serde_json::from_str(
                r#"{
                "UpscaleModelLoader": {"input": {"required": {"model_name": [["4x-UltraSharp.pth", "4x_foolhardy.pth"]]}}, "output": ["UPSCALE_MODEL"]},
                "ImageUpscaleWithModel": {"input": {"required": {"upscale_model": ["UPSCALE_MODEL"], "image": ["IMAGE"]}}, "output": ["IMAGE"]},
                "ImageScaleBy": {"input": {"required": {"image": ["IMAGE"], "upscale_method": [["nearest-exact", "lanczos"]], "scale_by": ["FLOAT", {"default": 1.0}]}}, "output": ["IMAGE"]},
                "UltralyticsDetectorProvider": {"input": {"required": {"model_name": [["bbox/face_yolov8m.pt", "segm/person.pt"]]}}, "output": ["BBOX_DETECTOR", "SEGM_DETECTOR"]},
                "FaceDetailer": {"input": {"required": {
                    "image": ["IMAGE"], "model": ["MODEL"], "clip": ["CLIP"], "vae": ["VAE"],
                    "positive": ["CONDITIONING"], "negative": ["CONDITIONING"], "bbox_detector": ["BBOX_DETECTOR"],
                    "seed": ["INT", {"default": 0}], "steps": ["INT", {"default": 20}], "cfg": ["FLOAT", {"default": 8.0}],
                    "sampler_name": [["euler"]], "scheduler": [["normal"]], "denoise": ["FLOAT", {"default": 0.5}],
                    "guide_size": ["FLOAT", {"default": 384.0}], "guide_size_for": ["BOOLEAN", {"default": true}],
                    "max_size": ["FLOAT", {"default": 1024.0}], "feather": ["INT", {"default": 5}],
                    "noise_mask": ["BOOLEAN", {"default": true}], "force_inpaint": ["BOOLEAN", {"default": true}],
                    "bbox_threshold": ["FLOAT", {"default": 0.5}], "bbox_dilation": ["INT", {"default": 10}],
                    "bbox_crop_factor": ["FLOAT", {"default": 3.0}], "drop_size": ["INT", {"default": 10}],
                    "wildcard": ["STRING", {"default": ""}], "cycle": ["INT", {"default": 1}]
                }, "optional": {"sam_model_opt": ["SAM_MODEL"]}}, "output": ["IMAGE"]},
                "ImageSharpen": {"input": {"required": {"image": ["IMAGE"], "sharpen_radius": ["INT", {"default": 1}], "sigma": ["FLOAT", {"default": 1.0}], "alpha": ["FLOAT", {"default": 1.0}]}}, "output": ["IMAGE"]}
            }"#,
            )
            .unwrap(),
        )
    }

    fn params() -> Params {
        Params {
            checkpoint: "sd.safetensors".into(),
            positive: "a cat".into(),
            seed: 42,
            steps: 25,
            cfg: 6.5,
            mode: Mode::Txt2Img,
            ..Default::default()
        }
    }

    fn ctx(g: &WorkflowGraph) -> Ctx {
        // Stand-in base graph; only the handles matter.
        let n = g.add_dynamic(WorkflowNode {
            inputs: HashMap::new(),
            class_type: "Base".into(),
            meta: None,
        });
        Ctx {
            image: ImageOut::from_dynamic(n, 0),
            latent: LatentOut::from_dynamic(n, 1),
            model: ModelOut::from_dynamic(n, 2),
            clip: ClipOut::from_dynamic(n, 3),
            vae: VaeOut::from_dynamic(n, 4),
            positive: ConditioningOut::from_dynamic(n, 5),
            negative: ConditioningOut::from_dynamic(n, 6),
        }
    }

    #[test]
    fn every_builtin_parses_and_validates() {
        let set = AppSet::builtin();
        assert!(set.bad.is_empty(), "bad builtins: {:?}", set.bad);
        assert_eq!(set.by_id.len(), BUILTIN.len());
        for def in set.by_id.values() {
            assert!(!def.name.is_empty(), "{} has no name", def.id);
            assert!(!def.group.is_empty(), "{} has no group", def.id);
        }
    }

    #[test]
    fn refs_parse() {
        let p = |s: &str| Ref::parse(s.strip_prefix('$').unwrap());
        assert_eq!(p("$image").unwrap(), Ref::Image);
        assert_eq!(p("$seed").unwrap(), Ref::Seed(0));
        assert_eq!(p("$seed+3").unwrap(), Ref::Seed(3));
        assert_eq!(p("$param:steps").unwrap(), Ref::Param(ParamField::Steps));
        assert_eq!(p("$knob:denoise").unwrap(), Ref::Knob("denoise".into()));
        assert_eq!(
            p("$node:loader").unwrap(),
            Ref::Node(LocalRef { node: "loader".into(), slot: 0 })
        );
        assert_eq!(
            p("$node:det:1").unwrap(),
            Ref::Node(LocalRef { node: "det".into(), slot: 1 })
        );
        assert!(p("$nope").is_err());
        assert!(p("$param:bogus").is_err());
        assert!(p("$node:det:x").is_err());
    }

    #[test]
    fn dollar_escape_is_a_literal() {
        assert!(as_ref(&Value::from("$$image")).is_none());
        assert_eq!(unescape(&Value::from("$$image")).unwrap(), Value::from("$image"));
        assert!(as_ref(&Value::from("plain")).is_none());
    }

    #[test]
    fn upscale_emits_two_wired_nodes() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let base = c.image.0.node_id;
        let steps = vec![AppStep::new(set.get("upscale.model").unwrap())];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.applied, vec!["Upscale (model)"]);

        let wf = g.borrow();
        let up = wf.0.get(&c.image.0.node_id).unwrap();
        assert_eq!(up.class_type, "ImageUpscaleWithModel");
        // The chain's input is the base image, and the loader feeds the upscaler.
        assert_eq!(up.inputs["image"], WorkflowInput::slot(base, 0));
        let (loader, _) = up.inputs["upscale_model"].as_slot().unwrap();
        assert_eq!(wf.0[&loader].class_type, "UpscaleModelLoader");
        assert_eq!(
            wf.0[&loader].inputs["model_name"],
            WorkflowInput::String("4x-UltraSharp.pth".into())
        );
    }

    #[test]
    fn steps_chain_in_order() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let base = c.image.0.node_id;
        let steps = vec![
            AppStep::new(set.get("face.detailer").unwrap()),
            AppStep::new(set.get("upscale.model").unwrap()),
        ];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);

        let wf = g.borrow();
        // Last step is the upscaler, fed by the detailer, fed by the base image.
        let up = &wf.0[&c.image.0.node_id];
        assert_eq!(up.class_type, "ImageUpscaleWithModel");
        let (fd, _) = up.inputs["image"].as_slot().unwrap();
        assert_eq!(wf.0[&fd].class_type, "FaceDetailer");
        let (src, _) = wf.0[&fd].inputs["image"].as_slot().unwrap();
        assert_eq!(src, base);
    }

    #[test]
    fn face_detailer_inherits_params_and_offsets_seed() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        apply(&g, &mut c, &steps, &set, &schemas, &params());

        let wf = g.borrow();
        let fd = &wf.0[&c.image.0.node_id];
        assert_eq!(fd.inputs["steps"], WorkflowInput::I64(25));
        assert_eq!(fd.inputs["cfg"], WorkflowInput::F64(6.5));
        assert_eq!(fd.inputs["seed"], WorkflowInput::I64(43));
        // MODEL/CLIP/VAE and both CONDITIONING handles come from the base graph.
        for socket in ["model", "clip", "vae", "positive", "negative"] {
            assert!(fd.inputs[socket].as_slot().is_some(), "{socket} not wired");
        }
    }

    #[test]
    fn ints_never_serialize_as_floats() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        apply(&g, &mut c, &steps, &set, &schemas, &params());
        let json = serde_json::to_string(&*g.borrow()).unwrap();
        assert!(json.contains("\"steps\":25"), "steps not an integer: {json}");
        assert!(!json.contains("\"steps\":25.0"));
        // guide_size is declared FLOAT by Impact Pack even though the knob is an int slider,
        // so the schema wins over the knob's type.
        let fd = &g.borrow().0[&c.image.0.node_id];
        assert_eq!(fd.inputs["guide_size"], WorkflowInput::F64(384.0));
        assert_eq!(fd.inputs["feather"], WorkflowInput::I64(5));
    }

    #[test]
    fn optional_requirement_absent_drops_its_node_and_refs() {
        let set = AppSet::builtin();
        let schemas = schemas(); // has no SAMLoader
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.skipped.is_empty());
        let wf = g.borrow();
        let fd = &wf.0[&c.image.0.node_id];
        assert!(!fd.inputs.contains_key("sam_model_opt"), "sam input should be unset");
        assert!(!wf.0.values().any(|n| n.class_type == "SAMLoader"));
    }

    #[test]
    fn unsupported_inputs_are_dropped_and_reported() {
        let mut raw: serde_json::Value = serde_json::from_str(
            r#"{"ImageSharpen": {"input": {"required": {"image": ["IMAGE"], "sharpen_radius": ["INT", {"default": 1}]}}, "output": ["IMAGE"]}}"#,
        )
        .unwrap();
        raw.as_object_mut().unwrap();
        let schemas = crate::schema::parse(&raw);
        let set = AppSet::builtin();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("sharpen").unwrap())];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.applied.contains(&"Sharpen".to_string()));
        // sigma and alpha are absent from this build's schema.
        assert!(report.dropped.iter().any(|(c, i)| c == "ImageSharpen" && i == "sigma"));
        assert!(report.note().contains("sigma"));
    }

    #[test]
    fn missing_class_skips_loudly_without_failing() {
        let schemas = crate::schema::parse(&serde_json::from_str(r#"{}"#).unwrap());
        let set = AppSet::builtin();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let before = c.image.0.node_id;
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.applied.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].1.contains("Impact-Pack"));
        // The image handle is untouched, so the base result still saves.
        assert_eq!(c.image.0.node_id, before);
    }

    #[test]
    fn broken_schema_is_distinguished_from_missing() {
        let mut schemas = schemas();
        schemas.nodes.remove("FaceDetailer");
        schemas.skipped.push(("FaceDetailer".into(), "node value is a string".into()));
        let set = AppSet::builtin();
        let def = set.get("face.detailer").unwrap();
        let st = status(def, None, Some(&schemas));
        assert!(matches!(st, Status::Broken(ref b) if b[0].0 == "FaceDetailer"));
        assert!(st.chip().contains("failed to parse"));
    }

    #[test]
    fn absent_model_degrades_but_stays_runnable() {
        let set = AppSet::builtin();
        let mut schemas = schemas();
        // Server has upscale models, just not the one the default names.
        schemas.nodes.remove("UpscaleModelLoader");
        let raw: serde_json::Value = serde_json::from_str(
            r#"{"UpscaleModelLoader": {"input": {"required": {"model_name": [["4x_other.pth"]]}}, "output": ["UPSCALE_MODEL"]}}"#,
        )
        .unwrap();
        let extra = crate::schema::parse(&raw);
        schemas.nodes.extend(extra.nodes);
        let st = status(set.get("upscale.model").unwrap(), None, Some(&schemas));
        assert!(st.runnable());
        assert!(matches!(st, Status::Degraded(_)), "{st:?}");
    }

    #[test]
    fn disabled_steps_are_not_emitted() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let base = c.image.0.node_id;
        let mut step = AppStep::new(set.get("upscale.model").unwrap());
        step.enabled = false;
        let report = apply(&g, &mut c, &[step], &set, &schemas, &params());
        assert!(report.applied.is_empty() && report.skipped.is_empty());
        assert_eq!(c.image.0.node_id, base);
    }

    #[test]
    fn knob_overrides_reach_the_node() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let mut step = AppStep::new(set.get("upscale.model").unwrap());
        step.values.insert("model_name".into(), Value::from("4x_foolhardy.pth"));
        apply(&g, &mut c, &[step], &set, &schemas, &params());
        let wf = g.borrow();
        let (loader, _) = wf.0[&c.image.0.node_id].inputs["upscale_model"].as_slot().unwrap();
        assert_eq!(
            wf.0[&loader].inputs["model_name"],
            WorkflowInput::String("4x_foolhardy.pth".into())
        );
    }

    #[test]
    fn forward_reference_is_rejected_at_load() {
        let mut set = AppSet::default();
        set.insert_json(
            "bad.json",
            r#"{"id":"bad","name":"Bad","nodes":[
                {"id":"a","class":"ImageSharpen","inputs":{"image":"$node:b"}},
                {"id":"b","class":"ImageSharpen","inputs":{"image":"$image"}}],
              "output":{"node":"a"}}"#,
        );
        assert!(set.by_id.is_empty());
        assert!(set.bad[0].1.contains("before it is declared"));
    }

    #[test]
    fn emitted_graph_is_acyclic() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![
            AppStep::new(set.get("face.detailer").unwrap()),
            AppStep::new(set.get("sharpen").unwrap()),
            AppStep::new(set.get("upscale.model").unwrap()),
        ];
        apply(&g, &mut c, &steps, &set, &schemas, &params());
        // topological_sort_with_depth panics on a cycle.
        let wf = g.borrow().clone();
        assert_eq!(wf.topological_sort().len(), wf.0.len());
    }

    /// A preset carried from another server names a model this one lacks. Sending it verbatim
    /// would fail /prompt validation for the WHOLE graph, losing the base image too.
    #[test]
    fn uninstalled_enum_value_is_substituted_not_sent() {
        let set = AppSet::builtin();
        let schemas = schemas(); // offers 4x-UltraSharp.pth and 4x_foolhardy.pth
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let mut step = AppStep::new(set.get("upscale.model").unwrap());
        step.values.insert("model_name".into(), Value::from("4x-NotHere.pth"));
        let report = apply(&g, &mut c, &[step], &set, &schemas, &params());

        assert!(report.skipped.is_empty());
        let wf = g.borrow();
        let (loader, _) = wf.0[&c.image.0.node_id].inputs["upscale_model"].as_slot().unwrap();
        assert_eq!(
            wf.0[&loader].inputs["model_name"],
            WorkflowInput::String("4x-UltraSharp.pth".into())
        );
        assert_eq!(report.substituted.len(), 1);
        assert!(report.note().contains("4x-NotHere.pth"), "{}", report.note());
    }

    /// The fallback has to honour the knob's prefix — a face detector must not become a
    /// person-segmentation model just because that option sorts first.
    #[test]
    fn enum_fallback_respects_the_knobs_prefix() {
        let set = AppSet::builtin();
        let mut schemas = schemas();
        schemas.nodes.remove("UltralyticsDetectorProvider");
        let extra = crate::schema::parse(
            &serde_json::from_str(
                r#"{"UltralyticsDetectorProvider": {"input": {"required": {"model_name": [["segm/person_yolov8m.pt", "bbox/face_yolov8n.pt"]]}}, "output": ["BBOX_DETECTOR", "SEGM_DETECTOR"]}}"#,
            )
            .unwrap(),
        );
        schemas.nodes.extend(extra.nodes);

        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        apply(&g, &mut c, &steps, &set, &schemas, &params());

        let wf = g.borrow();
        let (det, _) = wf.0[&c.image.0.node_id].inputs["bbox_detector"].as_slot().unwrap();
        assert_eq!(
            wf.0[&det].inputs["model_name"],
            WorkflowInput::String("bbox/face_yolov8n.pt".into()),
            "fallback ignored the bbox/ prefix"
        );
    }

    /// Plain literals on COMBO inputs get the same protection as knobs — FaceDetailer's
    /// hardcoded SAM checkpoint is the real case.
    #[test]
    fn uninstalled_literal_on_a_combo_is_substituted() {
        let mut schemas = schemas();
        let extra = crate::schema::parse(
            &serde_json::from_str(
                r#"{"SAMLoader": {"input": {"required": {"model_name": [["sam_vit_h_other.pth"]], "device_mode": [["AUTO"]]}}, "output": ["SAM_MODEL"]}}"#,
            )
            .unwrap(),
        );
        schemas.nodes.extend(extra.nodes);

        let set = AppSet::builtin();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps = vec![AppStep::new(set.get("face.detailer").unwrap())];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());

        let wf = g.borrow();
        let sam = wf.0.values().find(|n| n.class_type == "SAMLoader").expect("sam not emitted");
        assert_eq!(
            sam.inputs["model_name"],
            WorkflowInput::String("sam_vit_h_other.pth".into())
        );
        assert!(report.substituted.iter().any(|(at, _, _)| at == "SAMLoader.model_name"));
    }

    /// hires.fix re-renders from the base latent, so anything above it is discarded.
    #[test]
    fn latent_step_after_an_image_step_warns() {
        let set = AppSet::builtin();
        let mut schemas = schemas();
        let extra = crate::schema::parse(
            &serde_json::from_str(
                r#"{
                "LatentUpscaleBy": {"input": {"required": {"samples": ["LATENT"], "upscale_method": [["bislerp"]], "scale_by": ["FLOAT", {"default": 1.5}]}}, "output": ["LATENT"]},
                "KSampler": {"input": {"required": {"model": ["MODEL"], "positive": ["CONDITIONING"], "negative": ["CONDITIONING"], "latent_image": ["LATENT"], "seed": ["INT", {"default": 0}], "steps": ["INT", {"default": 20}], "cfg": ["FLOAT", {"default": 8.0}], "sampler_name": [["euler"]], "scheduler": [["normal"]], "denoise": ["FLOAT", {"default": 1.0}]}}, "output": ["LATENT"]},
                "VAEDecode": {"input": {"required": {"samples": ["LATENT"], "vae": ["VAE"]}}, "output": ["IMAGE"]}
            }"#,
            )
            .unwrap(),
        );
        schemas.nodes.extend(extra.nodes);
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);

        // Wrong order: upscale first, then hi-res fix.
        let steps = vec![
            AppStep::new(set.get("upscale.model").unwrap()),
            AppStep::new(set.get("hires.fix").unwrap()),
        ];
        let report = apply(&g, &mut c, &steps, &set, &schemas, &params());
        assert!(report.skipped.is_empty());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("Hi-res fix"));

        // Right order raises nothing.
        let g2 = WorkflowGraph::new();
        let mut c2 = ctx(&g2);
        let ok = apply(
            &g2,
            &mut c2,
            &[
                AppStep::new(set.get("hires.fix").unwrap()),
                AppStep::new(set.get("upscale.model").unwrap()),
            ],
            &set,
            &schemas,
            &params(),
        );
        assert!(ok.warnings.is_empty(), "{:?}", ok.warnings);
    }

    /// An `optional: true` require must not excuse an ungated node from existing.
    #[test]
    fn optional_require_cannot_mask_an_ungated_missing_class() {
        let mut set = AppSet::default();
        set.insert_json(
            "masked.json",
            r#"{"id":"masked","name":"Masked","group":"Finish",
              "requires":[{"class":"ImageSharpen","pack":"core","optional":true}],
              "nodes":[
                {"id":"a","class":"ImageSharpen","inputs":{"image":"$image"}},
                {"id":"b","class":"ImageScaleBy","inputs":{"image":"$node:a:0","scale_by":0.5}}],
              "output":{"node":"b"}}"#,
        );
        assert!(set.bad.is_empty(), "{:?}", set.bad);

        // Server has ImageScaleBy but not ImageSharpen.
        let mut schemas = schemas();
        schemas.nodes.remove("ImageSharpen");
        let def = set.get("masked").unwrap();
        let st = status(def, None, Some(&schemas));
        assert!(!st.runnable(), "ungated missing class reported as {st:?}");

        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let base = c.image.0.node_id;
        let report = apply(&g, &mut c, &[AppStep::new(def)], &set, &schemas, &params());
        assert_eq!(report.skipped.len(), 1);
        // The base image still saves rather than the whole prompt being rejected.
        assert_eq!(c.image.0.node_id, base);
    }

    #[test]
    fn plan_separates_literals_links_and_open_boundary_inputs() {
        let set = AppSet::builtin();
        let plan = set.get("face.detailer").unwrap().plan(None);
        assert_eq!(plan.len(), 3);

        let det = &plan[0];
        assert_eq!(det.class, "UltralyticsDetectorProvider");
        // The detector's model name is a knob, resolved to its default for the editor.
        assert_eq!(det.literals["model_name"], Value::from("bbox/face_yolov8m.pt"));

        let sam = &plan[1];
        assert_eq!(sam.optional.as_deref(), Some("SAMLoader"));

        let fd = &plan[2];
        assert_eq!(fd.class, "FaceDetailer");
        // Intra-app wires are links, not literals.
        assert!(fd.links.contains(&("bbox_detector".into(), "det".into(), 0)));
        assert!(!fd.literals.contains_key("bbox_detector"));
        // Handles the surrounding graph must supply are reported, never guessed at.
        let open: Vec<String> = fd.open.iter().map(|(_, r)| r.label()).collect();
        for want in ["image", "model", "clip", "vae", "positive", "negative"] {
            assert!(open.contains(&want.to_string()), "{want} not reported open");
        }
        // Plain literals survive as-is.
        assert_eq!(fd.literals["cycle"], Value::from(1));
        assert_eq!(fd.literals["wildcard"], Value::from(""));
    }

    #[test]
    fn plan_prefers_a_steps_configured_value_over_the_default() {
        let set = AppSet::builtin();
        let def = set.get("upscale.model").unwrap();
        let mut step = AppStep::new(def);
        step.values.insert("model_name".into(), Value::from("4x_foolhardy.pth"));
        let plan = def.plan(Some(&step));
        assert_eq!(plan[0].literals["model_name"], Value::from("4x_foolhardy.pth"));
    }

    #[test]
    fn every_slot_points_at_a_real_node() {
        let set = AppSet::builtin();
        let schemas = schemas();
        let g = WorkflowGraph::new();
        let mut c = ctx(&g);
        let steps: Vec<AppStep> = set.by_id.values().map(AppStep::new).collect();
        apply(&g, &mut c, &steps, &set, &schemas, &params());
        let wf = g.borrow();
        for (id, node) in &wf.0 {
            for (name, input) in &node.inputs {
                if let Some((dep, _)) = input.as_slot() {
                    assert!(
                        wf.0.contains_key(&dep),
                        "node {id} ({}) input {name} points at missing {dep}",
                        node.class_type
                    );
                }
            }
        }
    }
}
