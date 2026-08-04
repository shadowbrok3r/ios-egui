//! Persisted app configuration (JSON in the app's documents dir).

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::mount::MountPose;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePos {
    pub id: u8,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Sensing-server host:port (plain HTTP on the LAN).
    pub server: String,
    pub room_w: f64,
    pub room_h: f64,
    pub nodes: Vec<NodePos>,
    pub mount: MountPose,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: "192.168.1.10:8080".to_owned(),
            room_w: 5.0,
            room_h: 4.0,
            nodes: Vec::new(),
            mount: MountPose::default(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            std::fs::write(path, json).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = std::env::temp_dir().join("surveyor-config-test");
        let path = dir.join("config.json");
        let mut cfg = AppConfig::default();
        cfg.server = "10.0.0.5:8080".into();
        cfg.nodes.push(NodePos { id: 1, x: 0.0, y: 0.0 });
        cfg.mount.yaw_deg = 15.0;
        cfg.save(&path);
        let back = AppConfig::load(&path);
        assert_eq!(back.server, "10.0.0.5:8080");
        assert_eq!(back.nodes.len(), 1);
        assert!((back.mount.yaw_deg - 15.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = AppConfig::load(Path::new("/nonexistent/surveyor.json"));
        assert_eq!(cfg.room_w, 5.0);
    }
}
