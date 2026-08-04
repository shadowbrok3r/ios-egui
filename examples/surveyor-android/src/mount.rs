//! Radar mount pose, mirroring RuView's wifi-densepose-groundtruth crate:
//! room frame per ADR-152 (corner origin, +x along wall, +y into room, m);
//! radar frame has +y out of the antenna face, x lateral (flip per unit).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct MountPose {
    pub x_m: f64,
    pub y_m: f64,
    /// CCW degrees from the room's +y axis to the radar boresight.
    pub yaw_deg: f64,
    pub flip_x: bool,
}

impl Default for MountPose {
    fn default() -> Self {
        Self { x_m: 0.0, y_m: 0.0, yaw_deg: 0.0, flip_x: false }
    }
}

impl MountPose {
    pub fn to_room(&self, x_radar_m: f64, y_radar_m: f64) -> (f64, f64) {
        let xr = if self.flip_x { -x_radar_m } else { x_radar_m };
        let theta = self.yaw_deg.to_radians();
        let (sin, cos) = theta.sin_cos();
        (
            self.x_m + cos * xr - sin * y_radar_m,
            self.y_m + sin * xr + cos * y_radar_m,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_yaw_match_groundtruth_crate() {
        let m = MountPose::default();
        assert_eq!(m.to_room(0.5, 2.0), (0.5, 2.0));
        let m = MountPose { yaw_deg: 90.0, ..Default::default() };
        let (x, y) = m.to_room(0.0, 1.0);
        assert!((x - -1.0).abs() < 1e-9 && y.abs() < 1e-9);
    }
}
