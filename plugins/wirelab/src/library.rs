//! The board/component library: embedded from the desktop's assets at build
//! time, overlaid with whatever a connected desktop last served.

use std::collections::HashMap;

use egui_ios_plugin_sdk::abi;
use serde::{Deserialize, Serialize};
use wirelab_core::board::BoardProfile;
use wirelab_core::component::ComponentDef;
use wirelab_core::library::Library;

use crate::link::Ops;

include!(concat!(env!("OUT_DIR"), "/library.rs"));

/// Disk key for parts served by a desktop that the build didn't embed.
pub const LIBRARY_KEY: &str = "library";

#[derive(Serialize, Deserialize, Default)]
struct Cached {
    #[serde(default)]
    boards: Vec<BoardProfile>,
    #[serde(default)]
    components: Vec<ComponentDef>,
}

/// Parse the build-time assets into a library.
pub fn builtin() -> Library {
    let mut lib = Library::default();
    for text in BOARDS {
        if let Ok(b) = serde_json::from_str::<BoardProfile>(text) {
            lib.add_board(b);
        }
    }
    for text in DEFS {
        if let Ok(d) = serde_json::from_str::<ComponentDef>(text) {
            lib.add_component(d);
        }
    }
    lib
}

/// `builtin()` with the cached desktop parts applied over it.
pub fn load(ops: &dyn Ops) -> Library {
    let mut lib = builtin();
    let Ok(bytes) = ops.call("state.get", LIBRARY_KEY.as_bytes()) else { return lib };
    let Ok(Some(data)) = abi::decode::<Option<Vec<u8>>>(&bytes) else { return lib };
    if let Ok(cached) = serde_json::from_slice::<Cached>(&data) {
        for b in cached.boards {
            lib.add_board(b);
        }
        for c in cached.components {
            lib.add_component(c);
        }
    }
    lib
}

/// Whether `a` differs from `b` by serialized content (neither type is `PartialEq`).
fn differs<T: Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_vec(a).ok() != serde_json::to_vec(b).ok()
}

/// Overlay a desktop snapshot's parts; true when any part is new or changed.
pub fn merge(
    lib: &mut Library,
    profiles: &HashMap<String, BoardProfile>,
    defs: &HashMap<String, ComponentDef>,
) -> bool {
    let mut changed = false;
    for b in profiles.values() {
        changed |= lib.boards.get(&b.id).is_none_or(|old| differs(old, b));
        lib.add_board(b.clone());
    }
    for d in defs.values() {
        changed |= lib.components.get(&d.id).is_none_or(|old| differs(old, d));
        lib.add_component(d.clone());
    }
    changed
}

/// Persist the parts the build-time assets don't already carry verbatim.
///
/// A desktop-edited copy of a builtin part has to be stored too: dropping it reverts the project
/// to the stale build-time profile on the next launch, and wires onto pins only the edited version
/// declares then fail validation.
pub fn cache(ops: &dyn Ops, lib: &Library) {
    let base = builtin();
    let cached = Cached {
        boards: lib
            .boards
            .values()
            .filter(|b| base.boards.get(&b.id).is_none_or(|o| differs(o, *b)))
            .cloned()
            .collect(),
        components: lib
            .components
            .values()
            .filter(|c| base.components.get(&c.id).is_none_or(|o| differs(o, *c)))
            .cloned()
            .collect(),
    };
    let json = serde_json::to_vec(&cached).unwrap_or_default();
    let _ = ops.call("state.set", &abi::encode(&(LIBRARY_KEY.to_string(), json)));
}
