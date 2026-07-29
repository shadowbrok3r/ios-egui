//! The comfyui-android canvas look and auto-layout, lifted onto wirelab's flow graph.
//!
//! `wirelab_flow_ui::show` draws with a bare `SnarlStyle::new()`. Everything here is styling and
//! geometry over `Snarl<T>` alone — no knowledge of the node payload — so the plugin can drive it
//! against `wirelab_core::flow::NodeKind` without the shared crate changing.

use std::collections::{HashMap, HashSet};

use egui_ios_plugin_sdk::egui;
use wirelab_flow_ui::egui_snarl::{
    NodeId, Snarl,
    ui::{BackgroundPattern, NodeLayout, PinPlacement, SnarlStyle, WireStyle},
};

/// Zoom clamps, so a fit of a wide graph can't shrink nodes past legibility or blow one node up
/// to fill the canvas.
pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 2.5;

/// Stand-in extent for a node egui hasn't measured yet. Layout runs before the first paint, so
/// every node is laid out at this size and the result is loose rather than overlapping.
const NOMINAL_NODE: egui::Vec2 = egui::vec2(180.0, 100.0);

/// AMOLED canvas, bold axis-aligned traces, pins outside the node body.
pub fn style() -> SnarlStyle {
    let mut s = SnarlStyle::new();
    s.bg_frame = Some(egui::Frame::new().fill(egui::Color32::from_rgb(3, 3, 5)));
    // A tighter grid than snarl's default, so the canvas reads as graph paper the nodes sit on.
    s.bg_pattern = Some(BackgroundPattern::grid(egui::vec2(28.0, 28.0), 0.0));
    s.min_scale = Some(MIN_SCALE);
    s.max_scale = Some(MAX_SCALE);
    s.centering = Some(true);
    // Orthogonal wires with rounded corners — a structured "network diagram" look instead of
    // droopy beziers, and easier to follow where they run.
    s.wire_style = Some(WireStyle::AxisAligned { corner_radius: 8.0 });
    s.wire_width = Some(2.6);
    // Pins sit just outside the node body: their dots stop overlapping the pin labels, and they
    // become finger targets clear of the draggable node frame.
    s.pin_placement = Some(PinPlacement::Outside { margin: 3.0 });
    s.pin_size = Some(15.0);
    // Inputs above outputs rather than side by side. Snarl's default Coil layout measures both
    // columns against the full node width and sums them, so every node carries its output labels'
    // width as dead weight — which `arrange` then spaces its columns by.
    s.node_layout = Some(NodeLayout::sandwich());
    s
}

/// Bounding box of all nodes in graph space.
pub fn bounds<T>(snarl: &Snarl<T>) -> Option<egui::Rect> {
    let mut b: Option<egui::Rect> = None;
    for (_, pos, _) in snarl.nodes_pos_ids() {
        let r = egui::Rect::from_min_size(pos, NOMINAL_NODE);
        b = Some(b.map_or(r, |b| b.union(r)));
    }
    b
}

/// Compact layout: bands by longest-path depth, nodes stacked within each band, bands centred
/// across the flow, then relaxed toward straight wires. Returns the placed rects.
///
/// `vertical` runs execution top-to-bottom instead of left-to-right, so a deep graph lays out
/// along the screen's long axis rather than as a thin ribbon across it.
pub fn arrange<T>(
    snarl: &mut Snarl<T>,
    sizes: &HashMap<NodeId, egui::Vec2>,
    vertical: bool,
) -> Vec<egui::Rect> {
    // Gaps along the flow (between depths) and across it (between siblings in a depth).
    const FLOW_GAP: f32 = 60.0;
    const CROSS_GAP: f32 = 24.0;
    let flow = |v: egui::Vec2| if vertical { v.y } else { v.x };
    let cross = |v: egui::Vec2| if vertical { v.x } else { v.y };
    let at = |along: f32, across: f32| {
        if vertical { egui::pos2(across, along) } else { egui::pos2(along, across) }
    };

    let ids: Vec<NodeId> = snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
    let mut successors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut predecessors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in snarl.wires() {
        if from.node == to.node {
            continue;
        }
        successors.entry(from.node).or_default().push(to.node);
        predecessors.entry(to.node).or_default().push(from.node);
    }

    // Pseudo-topological order via iterative DFS post-order — robust to cycles, which a flow
    // graph can contain. Kahn's-style layering lets one cycle poison every downstream depth and
    // collapse the whole graph into a single column.
    let mut order: Vec<NodeId> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    for &start in &ids {
        if visited.contains(&start) {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((node, processed)) = stack.pop() {
            if processed {
                order.push(node);
                continue;
            }
            if !visited.insert(node) {
                continue;
            }
            stack.push((node, true));
            for &next in successors.get(&node).into_iter().flatten() {
                if !visited.contains(&next) {
                    stack.push((next, false));
                }
            }
        }
    }
    order.reverse(); // producers before consumers
    let topo: HashMap<NodeId, usize> = order.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Longest-path layer over forward edges only; back-edges wrap around rather than shoving
    // their target into a late column.
    let mut depth: HashMap<NodeId, usize> = ids.iter().map(|&id| (id, 0)).collect();
    for &node in &order {
        let d = depth[&node];
        for &next in successors.get(&node).into_iter().flatten() {
            if topo.get(&next).copied().unwrap_or(0) > topo[&node] {
                let e = depth.entry(next).or_insert(0);
                *e = (*e).max(d + 1);
            }
        }
    }
    let deepest = depth.values().copied().max().unwrap_or(0);
    let mut columns: Vec<Vec<NodeId>> = vec![Vec::new(); deepest + 1];
    for (id, _, _) in snarl.nodes_pos_ids() {
        columns[depth.get(&id).copied().unwrap_or(0)].push(id);
    }
    // Seed each column's order from the current layout, then reduce crossings with barycenter
    // sweeps so wires run mostly straight and execution reads down each column.
    for column in &mut columns {
        column.sort_by(|a, b| {
            let key = |id: &NodeId| {
                snarl
                    .get_node_info(*id)
                    .map(|n| if vertical { n.pos.x } else { n.pos.y })
                    .unwrap_or(0.0)
            };
            key(a).total_cmp(&key(b))
        });
    }
    let indices = |columns: &[Vec<NodeId>]| -> HashMap<NodeId, f32> {
        let mut m = HashMap::new();
        for column in columns {
            for (i, &id) in column.iter().enumerate() {
                m.insert(id, i as f32);
            }
        }
        m
    };
    let reorder = |column: &mut Vec<NodeId>,
                   neighbors: &HashMap<NodeId, Vec<NodeId>>,
                   idx: &HashMap<NodeId, f32>| {
        let mut keyed: Vec<(NodeId, f32)> = column
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let bary = match neighbors.get(&id) {
                    Some(ns) if !ns.is_empty() => {
                        ns.iter().filter_map(|n| idx.get(n)).sum::<f32>() / ns.len() as f32
                    }
                    _ => i as f32,
                };
                (id, bary)
            })
            .collect();
        keyed.sort_by(|a, b| a.1.total_cmp(&b.1));
        *column = keyed.into_iter().map(|(id, _)| id).collect();
    };
    for _ in 0..4 {
        let idx = indices(&columns);
        for d in 1..columns.len() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &predecessors, &idx);
            columns[d] = column;
        }
        let idx = indices(&columns);
        for d in (0..columns.len().saturating_sub(1)).rev() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &successors, &idx);
            columns[d] = column;
        }
    }

    let size_of = |id: NodeId| sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
    // Band offsets and thicknesses along the flow, from the deepest-extent node in each band.
    let mut col_x = Vec::with_capacity(columns.len());
    let mut col_w = Vec::with_capacity(columns.len());
    let mut x = 0.0f32;
    for column in &columns {
        col_x.push(x);
        let w = if column.is_empty() {
            0.0
        } else {
            column.iter().map(|&id| flow(size_of(id))).fold(1.0f32, f32::max)
        };
        col_w.push(w);
        if !column.is_empty() {
            x += w + FLOW_GAP;
        }
    }
    // Seed each node's cross-axis centre from a centred stack per band.
    let mut cy: HashMap<NodeId, f32> = HashMap::new();
    for column in &columns {
        let total: f32 =
            column.iter().map(|&id| cross(size_of(id)) + CROSS_GAP).sum::<f32>() - CROSS_GAP;
        let mut top = -total / 2.0;
        for &id in column {
            let h = cross(size_of(id));
            cy.insert(id, top + h / 2.0);
            top += h + CROSS_GAP;
        }
    }
    // Push each node along to keep CROSS_GAP from the one before it, preserving column order.
    let resolve = |column: &[NodeId], cy: &mut HashMap<NodeId, f32>| {
        for w in 1..column.len() {
            let (prev, cur) = (column[w - 1], column[w]);
            let min_c =
                cy[&prev] + cross(size_of(prev)) / 2.0 + CROSS_GAP + cross(size_of(cur)) / 2.0;
            if cy[&cur] < min_c {
                cy.insert(cur, min_c);
            }
        }
    };
    let column_mean = |column: &[NodeId], cy: &HashMap<NodeId, f32>| -> f32 {
        if column.is_empty() {
            return 0.0;
        }
        column.iter().filter_map(|id| cy.get(id)).sum::<f32>() / column.len() as f32
    };
    let edges: Vec<(NodeId, NodeId)> = successors
        .iter()
        .flat_map(|(from, tos)| tos.iter().map(|to| (*from, *to)))
        .collect();
    let wire_cost = |cy: &HashMap<NodeId, f32>| -> f32 {
        edges
            .iter()
            .filter_map(|(a, b)| Some((cy.get(a)?, cy.get(b)?)))
            .map(|(a, b)| (a - b).abs())
            .sum()
    };
    let vspan = |cy: &HashMap<NodeId, f32>| -> f32 {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (&id, &c) in cy {
            let h = cross(size_of(id));
            lo = lo.min(c - h / 2.0);
            hi = hi.max(c + h / 2.0);
        }
        if hi > lo { hi - lo } else { 0.0 }
    };
    // Straighter wires are worth some height, but only if they buy twice the straightening they
    // cost — otherwise the compact seeded stack wins.
    let seed_span = vspan(&cy);
    let score = |cy: &HashMap<NodeId, f32>| wire_cost(cy) + 0.5 * (vspan(cy) - seed_span).max(0.0);

    // Relax toward neighbour mean, then restore spacing. Every separation pass is
    // mean-preserving and the best iterate is kept, so relaxation can never return a layout
    // worse (or taller) than the seed it started from.
    let mut best = cy.clone();
    let mut best_score = score(&cy);
    for _ in 0..8 {
        for neighbors in [&predecessors, &successors] {
            for column in &columns {
                for &id in column {
                    let Some(ns) = neighbors.get(&id) else { continue };
                    // Averaged over the neighbours actually found: dividing by `ns.len()` with one
                    // missing biases toward 0, i.e. toward the top of the layout.
                    let (sum, found) = ns
                        .iter()
                        .filter_map(|n| cy.get(n))
                        .fold((0.0, 0usize), |(s, k), v| (s + v, k + 1));
                    if found > 0 {
                        cy.insert(id, sum / found as f32);
                    }
                }
            }
            for column in &columns {
                let before = column_mean(column, &cy);
                resolve(column, &mut cy);
                let drift = column_mean(column, &cy) - before;
                if drift != 0.0 {
                    for id in column {
                        if let Some(c) = cy.get_mut(id) {
                            *c -= drift;
                        }
                    }
                }
            }
        }
        let s = score(&cy);
        if s < best_score {
            best_score = s;
            best = cy.clone();
        }
    }
    let cy = best;

    let mut rects = Vec::new();
    for (d, column) in columns.iter().enumerate() {
        for &id in column {
            let size = size_of(id);
            // Centred within the band rather than pinned to its leading edge, so a thin node
            // beside a deep one doesn't sit against the edge with the difference left as a void.
            let pos = at(col_x[d] + (col_w[d] - flow(size)) / 2.0, cy[&id] - cross(size) / 2.0);
            if let Some(info) = snarl.get_node_info_mut(id) {
                info.pos = pos;
            }
            rects.push(egui::Rect::from_min_size(pos, size));
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two producers into one consumer: three nodes, two depths, and nothing overlapping.
    fn chain() -> Snarl<u32> {
        let mut s = Snarl::new();
        let a = s.insert_node(egui::pos2(500.0, 500.0), 0);
        let b = s.insert_node(egui::pos2(20.0, 900.0), 1);
        let c = s.insert_node(egui::pos2(-300.0, 40.0), 2);
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: a, output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: c, input: 0 },
        );
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: b, output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: c, input: 1 },
        );
        s
    }

    #[test]
    fn arrange_never_overlaps() {
        let mut s = chain();
        let rects = arrange(&mut s, &HashMap::new(), false);
        assert_eq!(rects.len(), 3);
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects[i].intersects(rects[j]), "{:?} overlaps {:?}", rects[i], rects[j]);
            }
        }
    }

    /// Producers land in an earlier band than the consumer they feed, whichever way the flow runs.
    #[test]
    fn arrange_puts_producers_before_consumers() {
        for vertical in [false, true] {
            let mut s = chain();
            let rects = arrange(&mut s, &HashMap::new(), vertical);
            let along = |r: &egui::Rect| if vertical { r.min.y } else { r.min.x };
            let consumer = rects.iter().map(along).fold(f32::MIN, f32::max);
            let producers: Vec<f32> = rects.iter().map(along).filter(|v| *v < consumer).collect();
            assert_eq!(producers.len(), 2, "vertical={vertical}");
        }
    }

    /// A graph whose wires form a cycle still lays out — the DFS ordering must not hang or
    /// collapse every node into one band.
    #[test]
    fn arrange_survives_a_cycle() {
        let mut s = chain();
        let ids: Vec<NodeId> = s.nodes_pos_ids().map(|(id, _, _)| id).collect();
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: ids[2], output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: ids[0], input: 0 },
        );
        assert_eq!(arrange(&mut s, &HashMap::new(), false).len(), 3);
    }

    #[test]
    fn an_empty_graph_arranges_to_nothing() {
        let mut s: Snarl<u32> = Snarl::new();
        assert!(arrange(&mut s, &HashMap::new(), false).is_empty());
        assert!(bounds(&s).is_none());
    }
}
