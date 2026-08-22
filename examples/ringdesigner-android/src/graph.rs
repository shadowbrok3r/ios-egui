//! The design's graph on the phone: an editor kept in step with
//! `design.graph`, evaluated on the build worker before every build.

use std::collections::BTreeMap;
use std::sync::Arc;

use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::castability::FieldReport;
use ringdesign_core::RingDesign;
use ringdesign_graph::eval::{evaluate_design, Evaluator};
use ringdesign_graph::graph::{Graph, GraphError, NodeId};
use ringdesign_graph::registry::Registry;
use ringdesign_graph_ui::Editor;

/// The editor and its bookkeeping against the design's own graph.
pub struct GraphState {
    pub reg: Arc<Registry>,
    pub ed: Option<Editor>,
    /// `design.graph` as last seen, so a replaced design replaces the editor.
    json: Option<serde_json::Value>,
    pub errors: Vec<String>,
    /// Pans and zooms only; no node moves, wires or edits.
    pub locked: bool,
}

impl Default for GraphState {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphState {
    pub fn new() -> Self {
        Self { reg: Arc::new(ringdesign_script::registry()), ed: None, json: None, errors: Vec::new(), locked: false }
    }

    pub fn is_driven(&self) -> bool {
        self.ed.is_some()
    }

    /// Keeps the editor in step with `design.graph`, whichever side moved.
    /// True when the editor was rebuilt or dropped.
    pub fn sync(&mut self, design: &RingDesign) -> bool {
        if design.graph == self.json {
            return false;
        }
        self.json = design.graph.clone();
        let parsed = design.graph.as_ref().and_then(|j| serde_json::from_value::<Graph>(j.clone()).ok());
        match parsed {
            Some(g) => match &mut self.ed {
                Some(ed) => ed.set_graph(g, &self.reg),
                None => {
                    let mut ed = Editor::new(g, &self.reg);
                    ed.editable = !self.locked;
                    ed.fit();
                    self.ed = Some(ed);
                }
            },
            None => self.ed = None,
        }
        true
    }

    /// The editor moved the graph: writes it into the design. True when the
    /// design changed.
    pub fn changed(&mut self, design: &mut RingDesign) -> bool {
        let Some(ed) = &self.ed else { return false };
        let json = serde_json::to_value(ed.graph()).ok();
        if design.graph == json {
            return false;
        }
        design.graph = json.clone();
        self.json = json;
        true
    }

    /// Makes `g` the design's graph.
    pub fn open(&mut self, design: &mut RingDesign, g: Graph) {
        design.graph = serde_json::to_value(&g).ok();
        self.sync(design);
    }

    /// Lifts the design into a graph that evaluates back to it exactly.
    pub fn convert(&mut self, design: &mut RingDesign, lib: &AlphaLibrary) -> Result<(), String> {
        let g = ringdesign_graph::lift::from_design(design, &self.reg, lib).map_err(|e| e.to_string())?;
        self.open(design, g);
        Ok(())
    }

    /// Drops the graph; the design stays as last evaluated. True when there
    /// was one.
    pub fn bake(&mut self, design: &mut RingDesign) -> bool {
        if design.graph.take().is_none() {
            return false;
        }
        self.json = None;
        self.ed = None;
        self.errors.clear();
        true
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        if let Some(ed) = &mut self.ed {
            ed.editable = !locked;
        }
    }

    /// The last evaluation's values and notes onto the editor's badges.
    pub fn apply(&mut self, done: &GraphDone) {
        self.errors = done.errors.iter().map(|e| e.to_string()).collect();
        if let Some(ed) = &mut self.ed {
            ed.set_values(&done.values);
            ed.set_diagnostics(&done.errors, &done.notes);
        }
    }
}

/// What evaluating a design's graph produced, handed back from the worker.
pub struct GraphDone {
    /// The evaluated design, carrying the graph; the job's own design when
    /// evaluation failed.
    pub design: RingDesign,
    pub values: BTreeMap<NodeId, BTreeMap<String, String>>,
    pub notes: BTreeMap<NodeId, Vec<String>>,
    pub errors: Vec<GraphError>,
    /// The verdict the evaluation already paid for.
    pub field: Option<FieldReport>,
    pub ok: bool,
}

/// The worker's evaluator, built once and cached across jobs.
pub struct GraphRunner {
    reg: Registry,
    evaluator: Evaluator,
}

impl Default for GraphRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphRunner {
    pub fn new() -> Self {
        Self { reg: ringdesign_script::registry(), evaluator: Evaluator::with_exprs(ringdesign_script::engine()) }
    }

    /// Evaluates `design.graph`, if it has one; the result keeps the graph.
    pub fn run(&mut self, design: &RingDesign, lib: &Arc<AlphaLibrary>) -> Option<GraphDone> {
        let json = design.graph.as_ref()?;
        let g: Graph = match serde_json::from_value(json.clone()) {
            Ok(g) => g,
            Err(e) => {
                return Some(GraphDone {
                    design: design.clone(),
                    values: BTreeMap::new(),
                    notes: BTreeMap::new(),
                    errors: vec![GraphError { node: None, message: format!("the design's graph does not parse: {e}") }],
                    field: None,
                    ok: false,
                });
            }
        };
        let epoch = Arc::as_ptr(lib) as usize as u64;
        match evaluate_design(&mut self.evaluator, &g, &self.reg, lib, epoch) {
            Ok(out) => {
                let mut d = (*out.design).clone();
                d.graph = design.graph.clone();
                let values = out
                    .report
                    .values
                    .iter()
                    .map(|(id, outs)| (*id, outs.iter().map(|(k, v)| (k.clone(), v.summary())).collect()))
                    .collect();
                let notes = out
                    .report
                    .status
                    .iter()
                    .filter(|(_, s)| !s.errors.is_empty() || !s.warnings.is_empty())
                    .map(|(id, s)| {
                        let mut lines: Vec<String> = s
                            .errors
                            .iter()
                            .map(|(i, m)| if s.items > 1 { format!("item {i}: {m}") } else { m.clone() })
                            .collect();
                        lines.extend(s.warnings.iter().cloned());
                        (*id, lines)
                    })
                    .collect();
                Some(GraphDone { design: d, values, notes, errors: Vec::new(), field: Some(out.field), ok: true })
            }
            Err(e) => Some(GraphDone {
                design: design.clone(),
                values: BTreeMap::new(),
                notes: BTreeMap::new(),
                errors: vec![e],
                field: None,
                ok: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringdesign_core::castability::Verdict;

    #[test]
    fn the_editor_follows_the_design_and_bake_drops_it() {
        let mut st = GraphState::new();
        let mut design = RingDesign::default();
        assert!(!st.sync(&design));
        assert!(!st.is_driven());
        st.open(&mut design, ringdesign_graph::templates::simple());
        assert!(st.is_driven());
        assert!(design.graph.is_some());
        assert!(!st.sync(&design), "already in step");
        st.set_locked(true);
        assert!(!st.ed.as_ref().unwrap().editable);
        let replaced = RingDesign::default();
        st.sync(&replaced);
        assert!(!st.is_driven(), "a design without a graph drops the editor");
        st.open(&mut design, ringdesign_graph::templates::simple());
        assert!(st.bake(&mut design));
        assert!(design.graph.is_none() && !st.is_driven());
        assert!(!st.bake(&mut design));
    }

    #[test]
    fn the_editor_writes_back_and_a_lift_round_trips() {
        let lib = AlphaLibrary::builtin();
        let mut st = GraphState::new();
        let mut design = RingDesign::default();
        st.convert(&mut design, &lib).unwrap();
        assert!(st.is_driven());
        assert!(!st.changed(&mut design), "nothing moved");
        let json = serde_json::to_value(st.ed.as_ref().unwrap().graph()).unwrap();
        assert_eq!(design.graph.as_ref(), Some(&json));
    }

    #[test]
    fn the_runner_evaluates_a_template_graph_and_keeps_it() {
        let lib = Arc::new(AlphaLibrary::builtin());
        let mut runner = GraphRunner::new();
        let mut design = RingDesign::default();
        assert!(runner.run(&design, &lib).is_none(), "no graph, nothing to run");
        let mut layered = false;
        for (name, g) in ringdesign_graph::templates::all() {
            design.graph = serde_json::to_value(&g).ok();
            let done = runner.run(&design, &lib).unwrap();
            assert!(done.ok, "{name}: {:?}", done.errors);
            assert_eq!(done.design.graph, design.graph, "the evaluated design keeps its graph");
            assert_ne!(done.field.as_ref().unwrap().verdict, Verdict::NotCastable, "{name}");
            assert!(!done.values.is_empty());
            layered |= !done.design.layers.layers.is_empty();
        }
        assert!(layered, "some template graph carries layers");
        design.graph = Some(serde_json::json!({"not": "a graph"}));
        let bad = runner.run(&design, &lib).unwrap();
        assert!(!bad.ok && bad.errors.len() == 1);
    }
}
