//! Lenient parsing of the sensing server's WebSocket JSON messages.
//!
//! The server's envelope grows fields release to release; extract only what
//! the UI displays and ignore everything else rather than failing hard —
//! same discipline as comfyui-android's object_info parser.

/// CSI localization estimate from a `sensing_update` message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocEstimate {
    pub x: f64,
    pub y: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SensingSnapshot {
    pub source: String,
    pub node_count: usize,
    pub presence: Option<bool>,
    pub estimated_persons: Option<u64>,
    pub localization: Option<LocEstimate>,
}

/// One radar target from an `mmwave_targets` message, converted to meters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarTargetM {
    pub x_m: f64,
    pub y_m: f64,
    pub speed_mps: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MmwaveSnapshot {
    pub node_id: u8,
    pub targets: Vec<RadarTargetM>,
}

/// A121 60 GHz presence packet: 1D micro-motion, no angle.
#[derive(Debug, Clone, PartialEq)]
pub struct A121Snapshot {
    pub node_id: u8,
    pub presence: bool,
    pub distance_m: f64,
    pub inter_score: f64,
    pub intra_score: f64,
    pub breathing_bpm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WsMsg {
    Sensing(SensingSnapshot),
    Mmwave(MmwaveSnapshot),
    A121(A121Snapshot),
    /// Recognized JSON with some other `type` tag — surfaced for the log view.
    Other(String),
}

pub fn parse(text: &str) -> Option<WsMsg> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let ty = v.get("type").and_then(|t| t.as_str())?;
    match ty {
        "sensing_update" => {
            let localization = v.get("localization").and_then(|l| {
                Some(LocEstimate {
                    x: l.get("x")?.as_f64()?,
                    y: l.get("y")?.as_f64()?,
                    confidence: l.get("confidence")?.as_f64()?,
                })
            });
            Some(WsMsg::Sensing(SensingSnapshot {
                source: v
                    .get("source")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .to_owned(),
                node_count: v
                    .get("nodes")
                    .and_then(|n| n.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0),
                presence: v
                    .get("classification")
                    .and_then(|c| c.get("presence"))
                    .and_then(|p| p.as_bool()),
                estimated_persons: v.get("estimated_persons").and_then(|e| e.as_u64()),
                localization,
            }))
        }
        "mmwave_targets" => {
            let node_id = v.get("node_id").and_then(|n| n.as_u64()).unwrap_or(0) as u8;
            let targets = v
                .get("targets")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            Some(RadarTargetM {
                                x_m: t.get("x_mm")?.as_f64()? / 1000.0,
                                y_m: t.get("y_mm")?.as_f64()? / 1000.0,
                                speed_mps: t.get("speed_cm_s")?.as_f64()? / 100.0,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(WsMsg::Mmwave(MmwaveSnapshot { node_id, targets }))
        }
        "a121_presence" => Some(WsMsg::A121(A121Snapshot {
            node_id: v.get("node_id").and_then(|n| n.as_u64()).unwrap_or(0) as u8,
            presence: v.get("presence").and_then(|p| p.as_bool()).unwrap_or(false),
            distance_m: v.get("distance_m").and_then(|d| d.as_f64()).unwrap_or(0.0),
            inter_score: v.get("inter_score").and_then(|d| d.as_f64()).unwrap_or(0.0),
            intra_score: v.get("intra_score").and_then(|d| d.as_f64()).unwrap_or(0.0),
            breathing_bpm: v.get("breathing_bpm").and_then(|d| d.as_f64()),
        })),
        other => Some(WsMsg::Other(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sensing_update_with_localization() {
        let msg = r#"{"type":"sensing_update","source":"esp32","nodes":[{"node_id":1},{"node_id":2}],
            "classification":{"presence":true},"estimated_persons":1,
            "localization":{"x":1.5,"y":2.0,"confidence":0.62,"timestamp":123.0}}"#;
        let Some(WsMsg::Sensing(s)) = parse(msg) else { panic!("expected sensing") };
        assert_eq!(s.node_count, 2);
        assert_eq!(s.presence, Some(true));
        let loc = s.localization.unwrap();
        assert_eq!((loc.x, loc.y), (1.5, 2.0));
    }

    #[test]
    fn sensing_without_localization_is_fine() {
        let msg = r#"{"type":"sensing_update","source":"simulated","nodes":[]}"#;
        let Some(WsMsg::Sensing(s)) = parse(msg) else { panic!() };
        assert!(s.localization.is_none());
        assert_eq!(s.node_count, 0);
    }

    #[test]
    fn parses_a121_presence() {
        let msg = r#"{"type":"a121_presence","node_id":9,"presence":true,"distance_m":0.54,"inter_score":8.5,"intra_score":1.4,"breathing_bpm":null,"seq":12,"ts_us":5}"#;
        let Some(WsMsg::A121(a)) = parse(msg) else { panic!("expected a121") };
        assert!(a.presence);
        assert_eq!(a.node_id, 9);
        assert!((a.distance_m - 0.54).abs() < 1e-9);
        assert!((a.inter_score - 8.5).abs() < 1e-9);
        assert_eq!(a.breathing_bpm, None);
    }

    #[test]
    fn a121_missing_fields_default_sane() {
        let Some(WsMsg::A121(a)) = parse(r#"{"type":"a121_presence"}"#) else { panic!() };
        assert!(!a.presence);
        assert_eq!(a.distance_m, 0.0);
    }

    #[test]
    fn parses_mmwave_targets_to_meters() {
        let msg = r#"{"type":"mmwave_targets","node_id":7,"seq":42,"ts_us":1,
            "targets":[{"x_mm":-782,"y_mm":1713,"speed_cm_s":-16}]}"#;
        let Some(WsMsg::Mmwave(m)) = parse(msg) else { panic!() };
        assert_eq!(m.node_id, 7);
        assert_eq!(m.targets.len(), 1);
        assert!((m.targets[0].x_m - -0.782).abs() < 1e-9);
        assert!((m.targets[0].y_m - 1.713).abs() < 1e-9);
        assert!((m.targets[0].speed_mps - -0.16).abs() < 1e-9);
    }

    #[test]
    fn unknown_types_and_garbage_are_tolerated() {
        assert!(matches!(
            parse(r#"{"type":"edge_fused_vitals","node_id":1}"#),
            Some(WsMsg::Other(t)) if t == "edge_fused_vitals"
        ));
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"no_type":1}"#).is_none());
    }
}
