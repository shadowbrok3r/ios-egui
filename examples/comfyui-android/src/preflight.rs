//! Pre-queue validation of an API-format workflow against the server schema. Catches the two
//! failures ComfyUI rejects a prompt for — a required typed socket with no source, and an enum
//! widget whose value isn't installed — so the user gets a clear message instead of an opaque
//! server error after the network round trip. Also snaps file-path/case mismatches to the one
//! installed file they obviously mean. Pure.

use rucomfyui::Workflow;
use rucomfyui::workflow::WorkflowInput;

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
