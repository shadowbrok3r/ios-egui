//! Contract test: the JSON the WireLab desktop serves at GET /project must
//! deserialize into the plugin's `Snapshot`, and the shared geometry must
//! resolve wire endpoints. Uses the real WireLab assets + a real example.

use std::collections::HashMap;

use wirelab_core::board::BoardProfile;
use wirelab_core::component::ComponentDef;
use wirelab_core::library::Library;
use wirelab_core::project::Project;

use wirelab_panel::view::Snapshot;

#[test]
fn desktop_project_json_round_trips_into_the_plugin() {
    let assets = std::path::Path::new("/home/shadowbroker/Desktop/wirelab/assets");
    let lib = Library::load(&assets.join("boards"), &assets.join("components")).expect("assets");
    let mut project = Project::load(
        &assets.join("examples/12-house-and-garage.wirelab.json"),
    )
    .expect("example");
    project.sync_active();

    // Mirror the desktop's Cmd::GetProjectSnapshot arm exactly.
    let mut profiles: HashMap<String, BoardProfile> = HashMap::new();
    let mut defs: HashMap<String, ComponentDef> = HashMap::new();
    for tab in &project.boards {
        if let Some(b) = lib.board(&tab.circuit.board_id) {
            profiles.insert(tab.circuit.board_id.clone(), b.clone());
        }
        for comp in tab.circuit.components.values() {
            if let Some(d) = lib.component(&comp.def_id) {
                defs.insert(comp.def_id.clone(), d.clone());
            }
        }
    }
    let flow_bases: HashMap<String, u64> = project
        .boards
        .iter()
        .map(|b| {
            // Same content-hash the desktop serves (mcp::flow_hash).
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            serde_json::to_string(&b.flow).unwrap_or_default().hash(&mut h);
            (b.id.to_string(), h.finish())
        })
        .collect();
    let json = serde_json::json!({
        "name": project.name,
        "active": project.active,
        "boards": project.boards,
        "profiles": profiles,
        "defs": defs,
        "flow_bases": flow_bases,
    });

    let snap: Snapshot = serde_json::from_value(json).expect("plugin parses the snapshot");
    assert_eq!(snap.boards.len(), 2);
    assert_eq!(snap.boards[0].name, "house");
    assert!(snap.profiles.contains_key("esp32-c5-devkitc-1"));
    assert!(snap.defs.contains_key("push-button"));
    assert!(snap.defs.contains_key("servo-sg90"));
    // Every board carries an edit base for optimistic-concurrency pushes.
    for tab in &snap.boards {
        assert!(snap.flow_bases.contains_key(&tab.id.to_string()));
    }

    // Every wire endpoint in every board resolves through shared geometry.
    for tab in &snap.boards {
        let profile = &snap.profiles[&tab.circuit.board_id];
        for w in tab.circuit.wires.values() {
            for ep in [&w.a, &w.b] {
                let pos = match ep {
                    wirelab_core::circuit::Endpoint::BoardPin { key } => {
                        let pin = profile
                            .pins
                            .iter()
                            .find(|p| &p.key == key)
                            .unwrap_or_else(|| panic!("pin {key}"));
                        wirelab_core::geometry::board_pin_world_pos(
                            profile,
                            pin,
                            tab.circuit.board_pos,
                        )
                    }
                    wirelab_core::circuit::Endpoint::Terminal { comp, terminal } => {
                        let c = &tab.circuit.components[comp];
                        let def = &snap.defs[&c.def_id];
                        let idx = def
                            .terminals
                            .iter()
                            .position(|t| &t.id == terminal)
                            .unwrap_or_else(|| panic!("terminal {terminal}"));
                        wirelab_core::geometry::terminal_world_pos(c, def, idx)
                    }
                };
                assert!(pos[0].is_finite() && pos[1].is_finite());
            }
        }
    }
}
