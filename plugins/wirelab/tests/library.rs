//! The build-time library must match what the desktop loads off disk, and the
//! local project store must survive a state.set / state.get round trip.

use std::cell::RefCell;
use std::collections::HashMap;

use egui_ios_plugin_sdk::abi;
use wirelab_core::library::Library;
use wirelab_panel::library;
use wirelab_panel::link::Ops;
use wirelab_panel::store::Store;

fn assets() -> std::path::PathBuf {
    match std::env::var("WIRELAB_ASSETS") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../EmbeddedApps/wirelab/assets"),
    }
}

#[test]
fn embedded_library_matches_the_desktop_assets() {
    let disk = {
        let a = assets();
        Library::load(&a.join("boards"), &a.join("components")).expect("assets")
    };
    let built = library::builtin();
    assert_eq!(
        built.boards.keys().collect::<Vec<_>>(),
        disk.boards.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        built.components.keys().collect::<Vec<_>>(),
        disk.components.keys().collect::<Vec<_>>()
    );
    assert!(!built.boards.is_empty() && !built.components.is_empty());
}

/// `state.get` / `state.set` over an in-memory map.
#[derive(Default)]
struct FakeState(RefCell<HashMap<String, Vec<u8>>>);

impl Ops for FakeState {
    fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        match op {
            "state.get" => {
                let key = String::from_utf8_lossy(payload).to_string();
                Ok(abi::encode(&self.0.borrow().get(&key).cloned()))
            }
            "state.set" => {
                let (key, data): (String, Vec<u8>) =
                    abi::decode(payload).map_err(|e| e.to_string())?;
                self.0.borrow_mut().insert(key, data);
                Ok(Vec::new())
            }
            other => Err(format!("unexpected op {other}")),
        }
    }
}

#[test]
fn local_projects_round_trip_through_state() {
    let ops = FakeState::default();
    let board = library::builtin().boards.keys().next().unwrap().clone();

    let mut store = Store::default();
    let id = store.create("Bench rig", &board, 1_000);
    store.active = Some(id.clone());
    store.save(&ops);

    let mut reloaded = Store::load(&ops);
    assert_eq!(reloaded.active.as_deref(), Some(id.as_str()));
    let p = reloaded.get(&id).expect("stored project");
    assert_eq!(p.name, "Bench rig");
    assert_eq!(p.boards.len(), 1);
    assert_eq!(p.boards[0].circuit.board_id, board);

    // Internally tagged types (flow nodes, rules, wire endpoints) survive JSON.
    let mut boards = p.boards.clone();
    boards[0].flow.nodes.push(wirelab_core::flow::FlowNode {
        kind: wirelab_core::flow::NodeKind::OnUart,
        pos: [0.0, 0.0],
    });
    reloaded.put(&id, "Bench rig", 0, &boards, 2_000);
    reloaded.rename(&id, "Bench rig 2", 3_000);
    reloaded.save(&ops);

    let again = Store::load(&ops);
    let p = again.get(&id).expect("stored project");
    assert_eq!(p.name, "Bench rig 2");
    assert_eq!(p.boards[0].flow.nodes.len(), 1);

    let mut last = again;
    last.delete(&id);
    assert!(last.get(&id).is_none());
    assert!(last.active.is_none());
}

/// A desktop-edited copy of a builtin board must survive a restart. Persisting only ids the build
/// didn't ship reverts the project to the stale profile, and wires onto pins only the edited
/// version declares then fail validation.
#[test]
fn an_edited_builtin_board_is_cached() {
    let ops = FakeState::default();
    let base = library::builtin();
    let untouched = base.boards.keys().next().unwrap().clone();
    let edited = base.boards.keys().nth(1).unwrap().clone();

    let mut lib = base.clone();
    let mut profiles = HashMap::new();
    let mut b = lib.board(&edited).unwrap().clone();
    let mut extra = b.pins[0].clone();
    extra.key = "GPIO99".into();
    b.pins.push(extra);
    profiles.insert(edited.clone(), b);
    assert!(library::merge(&mut lib, &profiles, &HashMap::new()), "an edit is a change");
    library::cache(&ops, &lib);

    // Reloading from scratch has to bring the edit back, and nothing else.
    let reloaded = library::load(&ops);
    assert!(reloaded.board(&edited).unwrap().pins.iter().any(|p| p.key == "GPIO99"));
    assert_eq!(
        reloaded.board(&untouched).unwrap().pins.len(),
        base.board(&untouched).unwrap().pins.len()
    );

    // An identical overlay is not a change, so it never triggers a rewrite.
    let same: HashMap<String, _> =
        base.boards.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut fresh = library::builtin();
    assert!(!library::merge(&mut fresh, &same, &HashMap::new()));
}
