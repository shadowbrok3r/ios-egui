//! Lenient, procedural ComfyUI node schemas parsed from raw `/object_info` JSON. Each node parses
//! independently and defensively: a malformed custom node degrades or lands in `skipped` instead
//! of failing the catalog. (rucomfyui's typed parse rejects the entire map when any one node
//! deviates — 32 of 2579 did on the reference server.)

use std::collections::BTreeMap;

use serde_json::Value;

/// The largest enum option string kept; longer values are preview-image blobs, not names.
const MAX_OPTION_LEN: usize = 1024;
/// Cap on options per enum input.
const MAX_OPTIONS: usize = 20_000;

#[derive(Debug, Default)]
pub struct SchemaSet {
    pub nodes: BTreeMap<String, NodeSchema>,
    /// Nodes that could not be parsed at all: (name, reason).
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct NodeSchema {
    pub name: String,
    pub display_name: String,
    pub category: String,
    pub description: String,
    /// Required inputs first, then optional, each in `input_order` where given.
    pub inputs: Vec<InputSchema>,
    pub outputs: Vec<OutputSchema>,
    pub output_node: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InputSchema {
    pub name: String,
    pub required: bool,
    pub kind: InputKind,
    pub tooltip: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum InputKind {
    /// Dropdown of choices; non-string options are stringified.
    Enum { options: Vec<String>, default: Option<String> },
    Int {
        default: i64,
        min: Option<i64>,
        max: Option<i64>,
        step: Option<i64>,
        /// The frontend appends a phantom `control_after_generate` widget value after this input.
        control: bool,
    },
    Float { default: f64, min: Option<f64>, max: Option<f64>, step: Option<f64> },
    Bool { default: bool },
    Text { default: String, multiline: bool },
    /// A typed socket fed by another node's output (MODEL, LATENT, IMAGE, ...).
    Connection { ty: String },
    /// Unrecognized spec shape, ignored but recorded.
    Opaque,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OutputSchema {
    pub ty: String,
    pub name: String,
    pub is_list: bool,
}

impl SchemaSet {
    pub fn enum_options(&self, node: &str, input: &str) -> Vec<String> {
        self.nodes
            .get(node)
            .and_then(|n| n.inputs.iter().find(|i| i.name == input))
            .and_then(|i| match &i.kind {
                InputKind::Enum { options, .. } => Some(options.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Checkpoint names: `CheckpointLoaderSimple.ckpt_name`, else the union across any
    /// `*CheckpointLoader*` node's `ckpt_name` (covers forks like `CheckpointLoader|pysssss`).
    pub fn checkpoints(&self) -> Vec<String> {
        let simple = self.enum_options("CheckpointLoaderSimple", "ckpt_name");
        if !simple.is_empty() {
            return simple;
        }
        let mut all = Vec::new();
        for (name, node) in &self.nodes {
            if !name.contains("CheckpointLoader") {
                continue;
            }
            for i in &node.inputs {
                if i.name == "ckpt_name"
                    && let InputKind::Enum { options, .. } = &i.kind
                {
                    for o in options {
                        if !all.contains(o) {
                            all.push(o.clone());
                        }
                    }
                }
            }
        }
        all
    }

    pub fn samplers(&self) -> Vec<String> {
        self.enum_options("KSampler", "sampler_name")
    }

    pub fn schedulers(&self) -> Vec<String> {
        self.enum_options("KSampler", "scheduler")
    }

    /// Installed LoRA filenames from `LoraLoader.lora_name` (falls back to `LoraLoaderModelOnly`).
    pub fn loras(&self) -> Vec<String> {
        let simple = self.enum_options("LoraLoader", "lora_name");
        if !simple.is_empty() {
            return simple;
        }
        self.enum_options("LoraLoaderModelOnly", "lora_name")
    }
}

pub fn parse(root: &Value) -> SchemaSet {
    let mut set = SchemaSet::default();
    let Some(map) = root.as_object() else {
        set.skipped.push(("<root>".into(), format!("object_info root is {}", kind_of(root))));
        return set;
    };
    for (name, v) in map {
        match parse_node(name, v) {
            Ok(node) => {
                set.nodes.insert(name.clone(), node);
            }
            Err(reason) => set.skipped.push((name.clone(), reason)),
        }
    }
    set
}

fn parse_node(name: &str, v: &Value) -> Result<NodeSchema, String> {
    let obj = v.as_object().ok_or_else(|| format!("node value is {}", kind_of(v)))?;
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);

    let mut inputs = Vec::new();
    for (bundle, required) in [("required", true), ("optional", false)] {
        let Some(specs) = obj
            .get("input")
            .and_then(|i| i.get(bundle))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for iname in ordered_keys(obj, bundle, specs) {
            inputs.push(parse_input(&iname, required, &specs[&iname]));
        }
    }

    Ok(NodeSchema {
        name: name.to_string(),
        display_name: text("display_name").unwrap_or_else(|| name.to_string()),
        category: text("category").unwrap_or_default(),
        description: text("description").unwrap_or_default(),
        inputs,
        outputs: parse_outputs(obj),
        output_node: obj.get("output_node").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// `input_order.<bundle>` entries that exist in the spec map, then any stragglers in map order.
fn ordered_keys(
    obj: &serde_json::Map<String, Value>,
    bundle: &str,
    specs: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let mut keys: Vec<String> = obj
        .get("input_order")
        .and_then(|o| o.get(bundle))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|k| specs.contains_key(*k))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for k in specs.keys() {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    keys
}

/// Parse one input spec. Observed shapes: `["TYPE"]`, `["TYPE", meta]`, `[[opts...], meta?]`,
/// bare `"TYPE"`, `[null]`, and specs with trailing extras. Never fails.
fn parse_input(name: &str, required: bool, spec: &Value) -> InputSchema {
    let (first, meta) = match spec {
        Value::String(_) => (spec.clone(), None),
        Value::Array(items) => (
            items.first().cloned().unwrap_or(Value::Null),
            items.get(1).and_then(Value::as_object).cloned(),
        ),
        _ => (Value::Null, None),
    };
    let meta = meta.unwrap_or_default();
    let kind = match &first {
        Value::Array(opts) => enum_kind(opts, &meta),
        Value::String(ty) => match ty.as_str() {
            "INT" => InputKind::Int {
                default: num_i64(meta.get("default")).unwrap_or(0),
                min: num_i64(meta.get("min")),
                max: num_i64(meta.get("max")),
                step: num_i64(meta.get("step")),
                control: meta.contains_key("control_after_generate"),
            },
            "FLOAT" => InputKind::Float {
                default: num_f64(meta.get("default")).unwrap_or(0.0),
                min: num_f64(meta.get("min")),
                max: num_f64(meta.get("max")),
                step: num_f64(meta.get("step")),
            },
            "BOOLEAN" => InputKind::Bool {
                default: meta.get("default").and_then(Value::as_bool).unwrap_or(false),
            },
            "STRING" => InputKind::Text {
                default: meta.get("default").and_then(Value::as_str).unwrap_or("").to_string(),
                multiline: meta.get("multiline").and_then(Value::as_bool).unwrap_or(false),
            },
            // Frontend-v3 combo spec: options ride in the meta object.
            "COMBO" => {
                let opts = meta.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
                enum_kind(&opts, &meta)
            }
            other => InputKind::Connection { ty: other.to_string() },
        },
        _ => InputKind::Opaque,
    };
    InputSchema {
        name: name.to_string(),
        required,
        kind,
        tooltip: meta.get("tooltip").and_then(Value::as_str).map(str::to_string),
    }
}

/// Stringify enum options; numbers/bools become their text form, `{content, image}` preview
/// entries keep only `content`, nulls and oversized blobs drop.
fn enum_kind(opts: &[Value], meta: &serde_json::Map<String, Value>) -> InputKind {
    let options: Vec<String> = opts
        .iter()
        .filter_map(option_text)
        .filter(|s| !s.is_empty() && s.len() <= MAX_OPTION_LEN)
        .take(MAX_OPTIONS)
        .collect();
    let default = meta.get("default").and_then(option_text);
    InputKind::Enum { options, default }
}

fn option_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Object(o) => o.get("content").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn parse_outputs(obj: &serde_json::Map<String, Value>) -> Vec<OutputSchema> {
    let tys = obj.get("output").and_then(Value::as_array).cloned().unwrap_or_default();
    let names = obj.get("output_name").and_then(Value::as_array).cloned().unwrap_or_default();
    let lists = obj.get("output_is_list").and_then(Value::as_array).cloned().unwrap_or_default();
    tys.iter()
        .enumerate()
        .map(|(i, t)| {
            let ty = match t {
                Value::String(s) => s.clone(),
                // rgthree-style combo output: the entry is the option list itself.
                Value::Array(_) => "COMBO".to_string(),
                other => kind_of(other).to_string(),
            };
            OutputSchema {
                name: names.get(i).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| ty.clone()),
                is_list: lists.get(i).and_then(Value::as_bool).unwrap_or(false),
                ty,
            }
        })
        .collect()
}

fn num_i64(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn num_f64(v: Option<&Value>) -> Option<f64> {
    v?.as_f64()
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ── Bridge to rucomfyui's typed ObjectInfo ────────────────────────────────────

use rucomfyui::object_info as oi;

/// Construct typed [`oi::ObjectInfo`] from the lenient schemas for consumers that require the
/// typed shape (the node graph editor). Every field rucomfyui's own parse would reject arrives
/// here already normalized, so construction cannot fail.
pub fn to_object_info(set: &SchemaSet) -> oi::ObjectInfo {
    set.nodes.values().map(|n| (n.name.clone(), to_object(n))).collect()
}

fn to_object(n: &NodeSchema) -> oi::Object {
    let mut required = BTreeMap::new();
    let mut optional = BTreeMap::new();
    let mut required_order = Vec::new();
    let mut optional_order = Vec::new();
    for i in &n.inputs {
        let entry = to_input(i);
        if i.required {
            required_order.push(i.name.clone());
            required.insert(i.name.clone(), entry);
        } else {
            optional_order.push(i.name.clone());
            optional.insert(i.name.clone(), entry);
        }
    }
    oi::Object {
        name: n.name.clone(),
        display_name: Some(n.display_name.clone()),
        description: n.description.clone(),
        python_module: String::new(),
        category: n.category.clone(),
        input: oi::ObjectInputBundle { required, optional: Some(optional) },
        input_order: oi::ObjectInputBundle {
            required: required_order,
            optional: Some(optional_order),
        },
        output: n.outputs.iter().map(|o| object_type(&o.ty)).collect(),
        output_is_list: n.outputs.iter().map(|o| Some(o.is_list)).collect(),
        output_name: n.outputs.iter().map(|o| o.name.clone()).collect(),
        output_node: n.output_node,
        output_tooltips: Vec::new(),
    }
}

fn to_input(i: &InputSchema) -> oi::ObjectInput {
    let (ty, typed) = match &i.kind {
        InputKind::Enum { options, .. } => (
            oi::ObjectInputType::Array(
                options.iter().cloned().map(oi::ObjectInputTypeArrayValue::String).collect(),
            ),
            None,
        ),
        InputKind::Int { default, min, max, step, .. } => {
            let min = min.unwrap_or(i64::MIN + 1);
            let mut max = max.unwrap_or(i64::MAX - 1);
            // The editor's convert_i64 routes an input down its u64 path (wrapping negative
            // values) when `min as u64 == 0` or `max as u64 >= i64::MAX`. Keep that path for
            // true unsigned ranges (min == 0, e.g. seeds) and nudge signed ranges off it: a
            // negative cap (CLIPSetLastLayer max -1) or an i64::MAX cap (PrimitiveInt) casts
            // into the trigger zone.
            if max < 0 {
                max = 0;
            } else if max == i64::MAX {
                max = i64::MAX - 1;
            }
            (
                oi::ObjectInputType::Typed(oi::ObjectType::Int),
                Some(oi::ObjectInputMetaTyped::Number(oi::ObjectInputMetaTypedNumber {
                    default: (*default).into(),
                    display: None,
                    max: max.into(),
                    min: min.into(),
                    round: None,
                    step: step.map(Into::into),
                })),
            )
        }
        InputKind::Float { default, min, max, step } => (
            oi::ObjectInputType::Typed(oi::ObjectType::Float),
            Some(oi::ObjectInputMetaTyped::Number(oi::ObjectInputMetaTypedNumber {
                default: (*default).into(),
                display: None,
                max: max.unwrap_or(f64::MAX).into(),
                min: min.unwrap_or(f64::MIN).into(),
                round: None,
                step: step.map(Into::into),
            })),
        ),
        InputKind::Bool { default } => (
            oi::ObjectInputType::Typed(oi::ObjectType::Boolean),
            Some(oi::ObjectInputMetaTyped::Boolean(oi::ObjectInputMetaTypedBoolean {
                default: *default,
            })),
        ),
        InputKind::Text { default, multiline } => (
            oi::ObjectInputType::Typed(oi::ObjectType::String),
            Some(oi::ObjectInputMetaTyped::String(oi::ObjectInputMetaTypedString {
                dynamic_prompts: None,
                multiline: Some(*multiline),
                default: Some(default.clone()),
            })),
        ),
        InputKind::Connection { ty } => (oi::ObjectInputType::Typed(object_type(ty)), None),
        InputKind::Opaque => (oi::ObjectInputType::Typed(object_type("*")), None),
    };
    oi::ObjectInput::InputWithMeta(
        ty,
        oi::ObjectInputMeta { tooltip: i.tooltip.clone(), typed },
    )
}

/// Map a type string onto [`oi::ObjectType`] via its serde renames (`"LATENT"` →
/// `ObjectType::Latent`), falling back to `Other` for anything unknown.
pub fn object_type(s: &str) -> oi::ObjectType {
    serde_json::from_value(Value::String(s.to_string()))
        .unwrap_or_else(|_| oi::ObjectType::Other(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> SchemaSet {
        parse(&serde_json::from_str(s).unwrap())
    }

    fn kind<'a>(set: &'a SchemaSet, node: &str, input: &str) -> &'a InputKind {
        &set.nodes[node].inputs.iter().find(|i| i.name == input).unwrap().kind
    }

    #[test]
    fn plain_node_parses_fully() {
        let set = parse_str(
            r#"{"KSampler": {
                "display_name": "KSampler", "description": "", "category": "sampling",
                "input": {"required": {
                    "model": ["MODEL"],
                    "steps": ["INT", {"default": 20, "min": 1, "max": 10000, "tooltip": "t"}],
                    "cfg": ["FLOAT", {"default": 8.0, "min": 0.0, "max": 100.0, "step": 0.1}],
                    "sampler_name": [["euler", "dpmpp_2m"]],
                    "text": ["STRING", {"default": "hi", "multiline": true}],
                    "tiled": ["BOOLEAN", {"default": true}]
                }},
                "input_order": {"required": ["model", "steps", "cfg", "sampler_name", "text", "tiled"]},
                "output": ["LATENT"], "output_is_list": [false], "output_name": ["LATENT"],
                "output_node": false
            }}"#,
        );
        assert!(set.skipped.is_empty());
        let n = &set.nodes["KSampler"];
        assert_eq!(n.inputs.len(), 6);
        assert!(matches!(kind(&set, "KSampler", "model"), InputKind::Connection { ty } if ty == "MODEL"));
        assert!(matches!(kind(&set, "KSampler", "steps"), InputKind::Int { default: 20, .. }));
        assert!(matches!(kind(&set, "KSampler", "cfg"), InputKind::Float { .. }));
        assert!(matches!(kind(&set, "KSampler", "sampler_name"), InputKind::Enum { options, .. } if options.len() == 2));
        assert!(matches!(kind(&set, "KSampler", "text"), InputKind::Text { multiline: true, .. }));
        assert!(matches!(kind(&set, "KSampler", "tiled"), InputKind::Bool { default: true }));
        assert_eq!(n.outputs.len(), 1);
        assert_eq!(set.samplers(), vec!["euler", "dpmpp_2m"]);
    }

    #[test]
    fn tolerates_observed_custom_node_shapes() {
        // Every shape that broke rucomfyui's typed parse on the reference server.
        let set = parse_str(
            r#"{
                "NoRequired": {"input": {"optional": {"x": ["INT", {}]}}, "output": [], "output_node": false},
                "BareString": {"input": {"required": {"reset": "BOOLEAN"}}, "output": []},
                "FloatOptions": {"input": {"required": {"scale": [[0.25, 0.5, 1.0], {"default": 1.0}]}}, "output": []},
                "IntOptions": {"input": {"required": {"block": [[128, 64], {"default": 128}]}}, "output": []},
                "BoolOptions": {"input": {"required": {"flag": [[false, true], {"default": true}]}}, "output": []},
                "MixedOptions": {"input": {"required": {"crop": [["disabled", "center", 0], {}]}}, "output": []},
                "NullSpec": {"input": {"required": {"scheduler": [null]}}, "output": []},
                "ArrayOutput": {"input": {"required": {}}, "output": [["a", "b"], "IMAGE"], "output_name": ["combo", "img"], "output_is_list": [false, false]},
                "NoInputAtAll": {"output": ["INT"]}
            }"#,
        );
        assert!(set.skipped.is_empty(), "skipped: {:?}", set.skipped);
        assert_eq!(set.nodes.len(), 9);
        assert!(matches!(kind(&set, "BareString", "reset"), InputKind::Bool { default: false }));
        assert!(matches!(kind(&set, "FloatOptions", "scale"), InputKind::Enum { options, default }
            if options == &["0.25", "0.5", "1.0"] && default.as_deref() == Some("1.0")));
        assert!(matches!(kind(&set, "IntOptions", "block"), InputKind::Enum { options, .. } if options == &["128", "64"]));
        assert!(matches!(kind(&set, "BoolOptions", "flag"), InputKind::Enum { options, .. } if options == &["false", "true"]));
        assert!(matches!(kind(&set, "MixedOptions", "crop"), InputKind::Enum { options, .. }
            if options == &["disabled", "center", "0"]));
        assert!(matches!(kind(&set, "NullSpec", "scheduler"), InputKind::Opaque));
        assert_eq!(set.nodes["ArrayOutput"].outputs[0].ty, "COMBO");
        assert_eq!(set.nodes["ArrayOutput"].outputs[1].ty, "IMAGE");
        assert!(!set.nodes["NoRequired"].inputs[0].required);
    }

    #[test]
    fn drops_blob_and_null_options_keeps_content() {
        let blob = "x".repeat(2000);
        let set = parse_str(&format!(
            r#"{{"Ckpt": {{"input": {{"required": {{"ckpt_name": [[
                "real.safetensors",
                {{"content": "preview.safetensors", "image": "{blob}"}},
                "{blob}",
                null
            ]]}}}}, "output": []}}}}"#
        ));
        let InputKind::Enum { options, .. } = kind(&set, "Ckpt", "ckpt_name") else {
            panic!("not an enum")
        };
        assert_eq!(options, &["real.safetensors", "preview.safetensors"]);
    }

    #[test]
    fn checkpoints_fall_back_to_other_loaders() {
        let set = parse_str(
            r#"{"CheckpointLoader|pysssss": {"input": {"required": {"ckpt_name": [["a.ckpt", "b.ckpt"]]}}, "output": []}}"#,
        );
        assert_eq!(set.checkpoints(), vec!["a.ckpt", "b.ckpt"]);
    }

    #[test]
    fn bridge_builds_typed_objects() {
        let set = parse_str(
            r#"{"KSampler": {
                "display_name": "KSampler", "category": "sampling",
                "input": {"required": {
                    "model": ["MODEL"],
                    "seed": ["INT", {"default": 0, "min": 0, "max": 18446744073709551615}],
                    "steps": ["INT", {"default": 20, "min": 1, "max": 10000}],
                    "sampler_name": [["euler", "dpmpp_2m"]]
                }},
                "input_order": {"required": ["model", "seed", "steps", "sampler_name"]},
                "output": ["LATENT"], "output_is_list": [false], "output_name": ["LATENT"],
                "output_node": false
            }}"#,
        );
        let info = to_object_info(&set);
        let obj = &info["KSampler"];
        assert_eq!(obj.display_name(), "KSampler");
        assert_eq!(obj.output, vec![oi::ObjectType::Latent]);
        let inputs: Vec<_> = obj.all_inputs().collect();
        assert_eq!(
            inputs.iter().map(|(n, _, _)| *n).collect::<Vec<_>>(),
            ["model", "seed", "steps", "sampler_name"]
        );
        assert!(matches!(
            inputs[0].1.as_input_type(),
            oi::ObjectInputType::Typed(oi::ObjectType::Model)
        ));
        assert!(matches!(
            inputs[3].1.as_input_type(),
            oi::ObjectInputType::Array(v) if v.len() == 2
        ));
        assert_eq!(object_type("weird_custom"), oi::ObjectType::Other("weird_custom".into()));
        assert_eq!(object_type("*"), oi::ObjectType::Wildcard);
    }

    /// Full-catalog test against a real server dump: set OBJECT_INFO_JSON to the fixture path.
    /// `OBJECT_INFO_JSON=/path/object_info.json cargo test -p comfyui_android -- --nocapture`
    #[test]
    fn real_object_info_fixture() {
        let Ok(path) = std::env::var("OBJECT_INFO_JSON") else {
            eprintln!("OBJECT_INFO_JSON not set; skipping fixture test");
            return;
        };
        let text = std::fs::read_to_string(&path).unwrap();
        let set = parse(&serde_json::from_str(&text).unwrap());
        println!("nodes={} skipped={}", set.nodes.len(), set.skipped.len());
        for (n, r) in &set.skipped {
            println!("  skipped {n}: {r}");
        }
        assert!(set.skipped.is_empty(), "every node should parse");
        let (cp, sa, sc) = (set.checkpoints(), set.samplers(), set.schedulers());
        println!("checkpoints={} samplers={} schedulers={}", cp.len(), sa.len(), sc.len());
        assert!(!cp.is_empty() && !sa.is_empty() && !sc.is_empty());
        let info = to_object_info(&set);
        assert_eq!(info.len(), set.nodes.len(), "bridge must cover every node");
    }
}
