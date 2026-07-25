//! Pre-queue validation of an API-format workflow against the server schema. Catches the two
//! failures ComfyUI rejects a prompt for — a required typed socket with no source, and an enum
//! widget whose value isn't installed — so the user gets a clear message instead of an opaque
//! server error after the network round trip. Also snaps file-path/case mismatches to the one
//! installed file they obviously mean, and restores the JSON type of numeric COMBO values. Pure.

use rucomfyui::Workflow;
use rucomfyui::workflow::WorkflowInput;
use serde_json::Value;

use crate::schema::{InputKind, SchemaSet};

/// A blocking problem that would fail server-side validation.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// The API/UI node id the server reports in its own error.
    pub node: u32,
    pub class: String,
    pub input: String,
    pub kind: ProblemKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProblemKind {
    /// A required typed socket has no connection (its source node was dropped or absent).
    MissingInput,
    /// An enum widget's value is not among the server's installed options.
    NotInstalled { value: String },
}

impl Problem {
    pub fn message(&self) -> String {
        match &self.kind {
            ProblemKind::MissingInput => format!(
                "{} (node {}): missing '{}' — its source node isn't on this server",
                self.class, self.node, self.input
            ),
            ProblemKind::NotInstalled { value } => format!(
                "{} (node {}): '{}' = \"{}\" isn't installed on this server",
                self.class,
                self.node,
                self.input,
                crate::types::file_basename(value)
            ),
        }
    }
}

/// The trailing path component, lower-cased, for loose file matching.
fn basename_key(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_ascii_lowercase()
}

/// A model-weight filename, whose enum list (checkpoints, LoRAs, VAEs, encoders…) is fixed when the
/// server starts — so a value that's absent is a real problem. Dynamic lists that our connect-time
/// snapshot can't see (uploaded input images: `LoadImage.image`) are deliberately excluded so a
/// freshly uploaded file is never mistaken for missing.
fn is_model_file(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    [".safetensors", ".ckpt", ".pt", ".pth", ".bin", ".gguf", ".sft", ".onnx"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Snap enum file values that miss the installed list to the one option sharing their basename
/// (case-insensitive) — scraped workflows carry a stale subfolder or casing. Only file-like values
/// (containing a `.`) with exactly one basename match are touched. Returns repair notes.
pub fn snap_installed_enums(wf: &mut Workflow, schemas: &SchemaSet) -> Vec<String> {
    let mut notes = Vec::new();
    for (_, node) in wf.0.iter_mut() {
        let Some(schema) = schemas.nodes.get(&node.class_type) else { continue };
        for input in &schema.inputs {
            let InputKind::Enum { options, .. } = &input.kind else { continue };
            if options.is_empty() {
                continue;
            }
            let Some(WorkflowInput::String(cur)) = node.inputs.get(&input.name) else { continue };
            if !is_model_file(cur) || options.iter().any(|o| o == cur) {
                continue;
            }
            let key = basename_key(cur);
            let mut hits = options.iter().filter(|o| basename_key(o) == key);
            let (Some(only), None) = (hits.next(), hits.next()) else { continue };
            let (only, name) = (only.clone(), input.name.clone());
            notes.push(format!("{}: {name} '{cur}' -> '{only}'", node.class_type));
            node.inputs.insert(name, WorkflowInput::String(only));
        }
    }
    notes
}

/// Restore the JSON type of COMBO values whose options are not strings (`RIFE VFI.scale_factor`
/// offers `[0.25, 0.5, 1.0, 2.0, 4.0]`). ComfyUI membership-tests a COMBO value with no coercion,
/// so `"1.0"` fails a list of numbers, while the editor and the UI-workflow converter both carry
/// every selection as display text. All-string option lists are left alone, as is a value naming
/// no option. Returns repair notes.
pub fn retype_combo_values(wf: &mut Workflow, schemas: &SchemaSet) -> Vec<String> {
    let mut notes = Vec::new();
    for node in wf.0.values_mut() {
        let Some(schema) = schemas.nodes.get(&node.class_type) else { continue };
        for input in &schema.inputs {
            let Some(text) = node.inputs.get(&input.name).and_then(scalar_text) else { continue };
            let Some(typed) = input.kind.enum_typed_value(&text) else { continue };
            let Some(want) = input_of(typed) else { continue };
            if node.inputs.get(&input.name) == Some(&want) {
                continue;
            }
            notes.push(format!("{}: {} '{text}' -> {typed}", node.class_type, input.name));
            node.inputs.insert(input.name.clone(), want);
        }
    }
    notes
}

/// The inverse of [`retype_combo_values`], for a workflow on its way INTO the graph editor: render
/// a numeric COMBO value as the display text the schema lists it under. The editor's dropdown only
/// binds to a string, so an API-format workflow's `scale_factor: 1.0` otherwise collapses the
/// widget to an empty text box, losing the option list and the value with it. Returns repair notes.
pub fn display_combo_values(wf: &mut Workflow, schemas: &SchemaSet) -> Vec<String> {
    let mut notes = Vec::new();
    for node in wf.0.values_mut() {
        let Some(schema) = schemas.nodes.get(&node.class_type) else { continue };
        for input in &schema.inputs {
            if !matches!(input.kind, InputKind::Enum { .. }) {
                continue;
            }
            let Some(cur) = node.inputs.get(&input.name) else { continue };
            if matches!(cur, WorkflowInput::String(_) | WorkflowInput::Slot(..)) {
                continue;
            }
            let Some(text) = scalar_text(cur) else { continue };
            let named = input.kind.enum_option_text(&text).unwrap_or(&text).to_string();
            notes.push(format!("{}: {} {text} -> '{named}'", node.class_type, input.name));
            node.inputs.insert(input.name.clone(), WorkflowInput::String(named));
        }
    }
    notes
}

/// The display text of a literal input; `None` for a connected socket.
fn scalar_text(v: &WorkflowInput) -> Option<String> {
    match v {
        WorkflowInput::String(s) => Some(s.clone()),
        WorkflowInput::I64(i) => Some(i.to_string()),
        WorkflowInput::U64(u) => Some(u.to_string()),
        WorkflowInput::F64(f) => Some(f.to_string()),
        WorkflowInput::Boolean(b) => Some(b.to_string()),
        WorkflowInput::Slot(..) => None,
    }
}

/// A scalar JSON value as the [`WorkflowInput`] that serializes back to it.
pub fn input_of(v: &Value) -> Option<WorkflowInput> {
    Some(match v {
        Value::String(s) => WorkflowInput::String(s.clone()),
        Value::Bool(b) => WorkflowInput::Boolean(*b),
        Value::Number(n) if n.is_i64() => WorkflowInput::I64(n.as_i64()?),
        Value::Number(n) if n.is_u64() => WorkflowInput::U64(n.as_u64()?),
        Value::Number(n) => WorkflowInput::F64(n.as_f64()?),
        Value::Null | Value::Array(_) | Value::Object(_) => return None,
    })
}

/// Validate `wf` against `schemas`. Nodes whose class the schema doesn't know are skipped (custom
/// nodes we can't judge). Node ids match what the server reports.
pub fn validate(wf: &Workflow, schemas: &SchemaSet) -> Vec<Problem> {
    let mut problems = Vec::new();
    for (id, node) in &wf.0 {
        let Some(schema) = schemas.nodes.get(&node.class_type) else { continue };
        for input in &schema.inputs {
            match &input.kind {
                InputKind::Connection { .. } if input.required => {
                    if !node.inputs.contains_key(&input.name) {
                        problems.push(Problem {
                            node: id.0,
                            class: node.class_type.clone(),
                            input: input.name.clone(),
                            kind: ProblemKind::MissingInput,
                        });
                    }
                }
                // Only model-weight values are judged: their lists are authoritative and static,
                // whereas non-file enums on exotic custom nodes can be incomplete in object_info and
                // dynamic input-image lists are stale in our snapshot — a false block is worse than
                // a rare server-side rejection.
                InputKind::Enum { options, .. } if !options.is_empty() => {
                    if let Some(WorkflowInput::String(v)) = node.inputs.get(&input.name)
                        && is_model_file(v)
                        && !options.iter().any(|o| o == v)
                    {
                        problems.push(Problem {
                            node: id.0,
                            class: node.class_type.clone(),
                            input: input.name.clone(),
                            kind: ProblemKind::NotInstalled { value: v.clone() },
                        });
                    }
                }
                _ => {}
            }
        }
    }
    // Missing sockets first (they read as the real breakage), then uninstalled files.
    problems.sort_by_key(|p| matches!(p.kind, ProblemKind::NotInstalled { .. }));
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use rucomfyui::workflow::{WorkflowNode, WorkflowNodeId};

    fn schemas() -> SchemaSet {
        crate::schema::parse(
            &serde_json::from_str(
                r#"{
            "CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [["real.safetensors", "SDXL/base.safetensors"]]}},
                "output": ["MODEL","CLIP","VAE"], "output_name": ["MODEL","CLIP","VAE"], "output_is_list": [false,false,false]},
            "LoraLoader": {"input": {"required": {"model": ["MODEL"], "clip": ["CLIP"], "lora_name": [["style.safetensors"]], "strength_model": ["FLOAT", {"default": 1.0}], "strength_clip": ["FLOAT", {"default": 1.0}]}},
                "output": ["MODEL","CLIP"], "output_name": ["MODEL","CLIP"], "output_is_list": [false,false]},
            "VAEEncode": {"input": {"required": {"pixels": ["IMAGE"], "vae": ["VAE"]}},
                "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
            "KSampler": {"input": {"required": {"sampler_name": [["euler", "dpmpp_2m"]]}},
                "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
            "LoadImage": {"input": {"required": {"image": [["existing.png"]]}},
                "output": ["IMAGE","MASK"], "output_name": ["IMAGE","MASK"], "output_is_list": [false,false]}
        }"#,
            )
            .unwrap(),
        )
    }

    fn node(class: &str, inputs: &[(&str, WorkflowInput)]) -> WorkflowNode {
        let mut n = WorkflowNode::new(class);
        for (k, v) in inputs {
            n.add_input((*k).to_string(), v.clone());
        }
        n
    }

    fn wf_of(id: u32, class: &str, inputs: &[(&str, WorkflowInput)]) -> Workflow {
        Workflow::new([(WorkflowNodeId(id), node(class, inputs))])
    }

    /// `RIFE VFI` as ComfyUI-Frame-Interpolation declares it: a COMBO of strings, one of floats,
    /// and one of bools.
    fn combo_schemas() -> SchemaSet {
        crate::schema::parse(
            &serde_json::from_str(
                r#"{"RIFE VFI": {"input": {"required": {
                "frames": ["IMAGE"],
                "ckpt_name": [["rife49.pth", "rife47.pth"]],
                "scale_factor": [[0.25, 0.5, 1.0, 2.0, 4.0], {"default": 1.0}],
                "fast_mode": [[false, true], {"default": false}]
            }}, "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        )
    }

    #[test]
    fn flags_missing_required_socket() {
        let wf = wf_of(5, "VAEEncode", &[("vae", WorkflowInput::slot(WorkflowNodeId(2), 0))]);
        let problems = validate(&wf, &schemas());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].node, 5);
        assert_eq!(problems[0].input, "pixels");
        assert_eq!(problems[0].kind, ProblemKind::MissingInput);
        assert!(problems[0].message().contains("missing 'pixels'"));
    }

    #[test]
    fn a_wired_socket_is_fine() {
        let wf = wf_of(
            5,
            "VAEEncode",
            &[
                ("pixels", WorkflowInput::slot(WorkflowNodeId(2), 0)),
                ("vae", WorkflowInput::slot(WorkflowNodeId(3), 0)),
            ],
        );
        assert!(validate(&wf, &schemas()).is_empty());
    }

    #[test]
    fn flags_uninstalled_enum_value() {
        let wf = wf_of(
            1,
            "CheckpointLoaderSimple",
            &[("ckpt_name", WorkflowInput::String("JANKU_v777.safetensors".into()))],
        );
        let problems = validate(&wf, &schemas());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, ProblemKind::NotInstalled { value: "JANKU_v777.safetensors".into() });
    }

    #[test]
    fn a_dynamic_input_image_is_not_flagged() {
        // LoadImage.image lists uploaded files, which our connect-time snapshot can't see; a value
        // absent from the snapshot may be a valid fresh upload, so it must not block the queue.
        let wf = wf_of(2, "LoadImage", &[("image", WorkflowInput::String("just_uploaded.png".into()))]);
        assert!(validate(&wf, &schemas()).is_empty());
    }

    #[test]
    fn a_non_file_enum_mismatch_is_not_flagged() {
        // A sampler the server lacks is left for its own (clear) rejection: object_info option lists
        // for non-file enums on custom nodes can be incomplete, so we don't risk a false block.
        let wf = wf_of(6, "KSampler", &[("sampler_name", WorkflowInput::String("res_multistep".into()))]);
        assert!(validate(&wf, &schemas()).is_empty());
    }

    #[test]
    fn a_connected_enum_input_is_not_flagged() {
        // A primitive feeding ckpt_name arrives as a Slot, not a literal; the server resolves it.
        let wf = wf_of(1, "CheckpointLoaderSimple", &[("ckpt_name", WorkflowInput::slot(WorkflowNodeId(9), 0))]);
        assert!(validate(&wf, &schemas()).is_empty());
    }

    /// `RIFE VFI.scale_factor` offers JSON numbers, and ComfyUI membership-tests a COMBO value
    /// without coercion — `"1.0"` is rejected where `1.0` passes. Every spelling the app might
    /// carry has to land on the option's own JSON.
    #[test]
    fn a_numeric_combo_leaves_as_a_number_however_it_arrived() {
        for arrived in [
            WorkflowInput::String("1.0".into()),
            WorkflowInput::String("1".into()),
            WorkflowInput::I64(1),
            WorkflowInput::U64(1),
            WorkflowInput::F64(1.0),
        ] {
            let mut wf = wf_of(25, "RIFE VFI", &[("scale_factor", arrived.clone())]);
            retype_combo_values(&mut wf, &combo_schemas());
            assert_eq!(
                wf.0[&WorkflowNodeId(25)].inputs["scale_factor"],
                WorkflowInput::F64(1.0),
                "arrived as {arrived:?}"
            );
        }
        // And it serializes as a JSON number, which is what the server actually reads.
        let mut wf = wf_of(25, "RIFE VFI", &[("scale_factor", WorkflowInput::String("1.0".into()))]);
        retype_combo_values(&mut wf, &combo_schemas());
        assert!(serde_json::to_string(&wf).unwrap().contains(r#""scale_factor":1.0"#));
    }

    /// The blast radius is COMBOs whose options are not strings; everything else is already right
    /// and must not be touched.
    #[test]
    fn retype_leaves_string_combos_sockets_and_unknown_values_alone() {
        let schemas = combo_schemas();
        let untouched: &[(&str, WorkflowInput)] = &[
            // A COMBO of strings: already correct.
            ("ckpt_name", WorkflowInput::String("rife49.pth".into())),
            // Names no option — the user's value survives rather than being reinterpreted.
            ("scale_factor", WorkflowInput::String("8.0".into())),
        ];
        for (name, v) in untouched {
            let mut wf = wf_of(1, "RIFE VFI", &[(name, v.clone())]);
            assert!(retype_combo_values(&mut wf, &schemas).is_empty(), "{name} was rewritten");
            assert_eq!(wf.0[&WorkflowNodeId(1)].inputs[*name], *v);
        }
        // A connected socket carries no literal to retype.
        let mut wf = wf_of(1, "RIFE VFI", &[("scale_factor", WorkflowInput::slot(WorkflowNodeId(9), 0))]);
        assert!(retype_combo_values(&mut wf, &schemas).is_empty());
        // An unknown class is left whole — we cannot judge a node we have no schema for.
        let mut wf = wf_of(1, "SomeCustomNode", &[("x", WorkflowInput::String("1.0".into()))]);
        assert!(retype_combo_values(&mut wf, &schemas).is_empty());
    }

    /// Bool COMBOs have the same defect: `"true"` is not `True` to Python's `in`.
    #[test]
    fn a_bool_combo_leaves_as_a_bool() {
        let mut wf = wf_of(3, "RIFE VFI", &[("fast_mode", WorkflowInput::String("true".into()))]);
        let notes = retype_combo_values(&mut wf, &combo_schemas());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(wf.0[&WorkflowNodeId(3)].inputs["fast_mode"], WorkflowInput::Boolean(true));
    }

    /// Loading the other way: the editor's dropdown binds to the schema's display text, so a
    /// number coming out of an API-format workflow has to be rendered as that text.
    #[test]
    fn display_renders_a_numeric_combo_as_its_option_text() {
        let schemas = combo_schemas();
        for (arrived, want) in [
            (WorkflowInput::F64(1.0), "1.0"),
            (WorkflowInput::I64(1), "1.0"),
            (WorkflowInput::F64(0.25), "0.25"),
            // Names no option: kept verbatim rather than snapped to something else.
            (WorkflowInput::F64(8.0), "8"),
        ] {
            let mut wf = wf_of(4, "RIFE VFI", &[("scale_factor", arrived.clone())]);
            display_combo_values(&mut wf, &schemas);
            assert_eq!(
                wf.0[&WorkflowNodeId(4)].inputs["scale_factor"],
                WorkflowInput::String(want.into()),
                "arrived as {arrived:?}"
            );
        }
        // A value already in display form, and a socket, are both left alone.
        let mut wf = wf_of(4, "RIFE VFI", &[
            ("scale_factor", WorkflowInput::String("2.0".into())),
            ("frames", WorkflowInput::slot(WorkflowNodeId(9), 0)),
        ]);
        assert!(display_combo_values(&mut wf, &schemas).is_empty());
    }

    /// Round trip: display text -> editor -> wire. The user's setting must survive both hops.
    #[test]
    fn display_and_retype_round_trip() {
        let schemas = combo_schemas();
        let mut wf = wf_of(5, "RIFE VFI", &[("scale_factor", WorkflowInput::F64(2.0))]);
        display_combo_values(&mut wf, &schemas);
        retype_combo_values(&mut wf, &schemas);
        assert_eq!(wf.0[&WorkflowNodeId(5)].inputs["scale_factor"], WorkflowInput::F64(2.0));
    }

    #[test]
    fn snaps_stale_subfolder_to_the_installed_file() {
        let mut wf = wf_of(
            1,
            "CheckpointLoaderSimple",
            &[("ckpt_name", WorkflowInput::String("oldsub/base.safetensors".into()))],
        );
        let notes = snap_installed_enums(&mut wf, &schemas());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(
            wf.0[&WorkflowNodeId(1)].inputs["ckpt_name"],
            WorkflowInput::String("SDXL/base.safetensors".into())
        );
        // Now that it snapped to an installed value, validation passes.
        assert!(validate(&wf, &schemas()).is_empty());
    }

    #[test]
    fn snap_leaves_a_genuinely_absent_file_for_validate() {
        let mut wf = wf_of(
            1,
            "CheckpointLoaderSimple",
            &[("ckpt_name", WorkflowInput::String("JANKU_v777.safetensors".into()))],
        );
        assert!(snap_installed_enums(&mut wf, &schemas()).is_empty());
        assert_eq!(validate(&wf, &schemas()).len(), 1);
    }
}
