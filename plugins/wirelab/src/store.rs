//! Projects created on this device, under the "projects" state key.
//!
//! JSON, not postcard: `Endpoint`, `Trigger`/`Action` and `NodeKind` are
//! internally tagged, which postcard serializes but cannot deserialize.

use egui_ios_plugin_sdk::abi;
use serde::{Deserialize, Serialize};
use wirelab_core::project::{BoardTab, Project};

use crate::link::Ops;

pub const PROJECTS_KEY: &str = "projects";

/// One on-device project. Board profiles and component defs are rehydrated
/// from the library on open, so they are not stored per project.
#[derive(Serialize, Deserialize, Clone)]
pub struct LocalProject {
    pub id: String,
    pub name: String,
    /// Session clock at the last write; orders the picker, not a wall time.
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub active: usize,
    pub boards: Vec<BoardTab>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default)]
    pub projects: Vec<LocalProject>,
    /// Project reopened on the next launch.
    #[serde(default)]
    pub active: Option<String>,
}

impl Store {
    pub fn load(ops: &dyn Ops) -> Store {
        let Ok(bytes) = ops.call("state.get", PROJECTS_KEY.as_bytes()) else {
            return Store::default();
        };
        let Ok(Some(data)) = abi::decode::<Option<Vec<u8>>>(&bytes) else {
            return Store::default();
        };
        serde_json::from_slice(&data).unwrap_or_default()
    }

    pub fn save(&self, ops: &dyn Ops) {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let _ = ops.call("state.set", &abi::encode(&(PROJECTS_KEY.to_string(), json)));
    }

    pub fn get(&self, id: &str) -> Option<&LocalProject> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Titles in most-recently-edited order.
    pub fn listing(&self) -> Vec<(String, String)> {
        let mut v: Vec<&LocalProject> = self.projects.iter().collect();
        v.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        v.into_iter().map(|p| (p.id.clone(), p.name.clone())).collect()
    }

    /// Add a one-board project and return its id.
    pub fn create(&mut self, name: &str, board_id: &str, now_ms: u64) -> String {
        let p = Project::new(name, board_id);
        let id = self.fresh_id(now_ms);
        self.projects.push(LocalProject {
            id: id.clone(),
            name: p.name,
            updated_ms: now_ms,
            active: p.active,
            boards: p.boards,
        });
        id
    }

    pub fn rename(&mut self, id: &str, name: &str, now_ms: u64) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
            p.updated_ms = now_ms;
        }
    }

    pub fn delete(&mut self, id: &str) {
        self.projects.retain(|p| p.id != id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

    /// Copy an open project's working state back over its stored entry.
    pub fn put(&mut self, id: &str, name: &str, active: usize, boards: &[BoardTab], now_ms: u64) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
            p.active = active;
            p.boards = boards.to_vec();
            p.updated_ms = now_ms;
        }
    }

    fn fresh_id(&self, now_ms: u64) -> String {
        let mut id = format!("p{now_ms:x}");
        let mut n = 1;
        while self.projects.iter().any(|p| p.id == id) {
            id = format!("p{now_ms:x}-{n}");
            n += 1;
        }
        id
    }
}
