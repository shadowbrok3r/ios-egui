//! The ComfyUI workflow embedded in a generated media file's own bytes.
//!
//! ComfyUI and `VHS_VideoCombine` write the graph into the output file's metadata — PNG `tEXt`
//! chunks, mp4 `moov/udta/meta/ilst` tags, Matroska `SimpleTag`s — and every one of those stores it
//! as contiguous JSON text. Scanning the bytes for a node-shaped JSON object therefore covers all
//! of them with one path, which is what makes a video's workflow reachable at all: comfy-gate's
//! `/gallery/api/workflow` scrapes stills only, and the viewer already holds the whole video file.

use serde_json::Value;

/// Tokens present in every ComfyUI graph: `class_type` in API format, the others in UI format.
const MARKERS: [&[u8]; 3] = [b"class_type", b"last_node_id", b"widgets_values"];
/// Text runs examined before giving up.
const MAX_RUNS: usize = 8;
/// `{` positions tried per text run.
const MAX_STARTS: usize = 64;
/// Keys the graph nests under, most useful first — UI format carries node positions.
const WRAPPERS: [&str; 3] = ["workflow", "prompt", "data"];
/// Ceiling on one text run. The largest graphs run to a few hundred KB; this only stops a marker
/// that happens to land in megabytes of printable video payload from costing a rescan of all of it.
const MAX_RUN: usize = 4 << 20;
/// Unwrapping depth, so a self-referential blob can't recurse forever.
const MAX_DEPTH: u8 = 6;

/// The ComfyUI graph embedded in `bytes`, re-serialized as JSON that
/// [`crate::gallery::parse_workflow_meta_for`] and the graph loader both accept. UI format wins over
/// API format when a file carries both.
pub fn embedded_workflow(bytes: &[u8]) -> Option<String> {
    let mut api: Option<Value> = None;
    let mut from = 0;
    for _ in 0..MAX_RUNS {
        let Some(marker) = MARKERS.iter().filter_map(|m| find(bytes, m, from)).min() else { break };
        let (start, end) = text_run(bytes, marker);
        from = end.max(marker + 1);
        if let Some(graph) = graph_in(&String::from_utf8_lossy(&bytes[start..end])) {
            if graph.get("nodes").is_some() {
                return serde_json::to_string(&graph).ok();
            }
            api = api.or(Some(graph));
        }
    }
    api.as_ref().and_then(|v| serde_json::to_string(v).ok())
}

/// The maximal run of text bytes containing `at`; the binary chunk headers around an embedded
/// string are not text, so the run is the stored string itself.
fn text_run(bytes: &[u8], at: usize) -> (usize, usize) {
    let binary = |&b: &u8| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t';
    let floor = at.saturating_sub(MAX_RUN);
    let ceiling = at.saturating_add(MAX_RUN).min(bytes.len());
    let start = bytes[floor..at].iter().rposition(binary).map_or(floor, |i| floor + i + 1);
    let end = bytes[at..ceiling].iter().position(binary).map_or(ceiling, |i| at + i);
    (start, end)
}

/// The first `{` in `text` that opens a balanced JSON object holding a graph.
fn graph_in(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .filter(|&i| bytes[i] == b'{')
        .take(MAX_STARTS)
        .filter_map(|i| balanced(text, i))
        .filter_map(|obj| serde_json::from_str::<Value>(obj).ok())
        .find_map(|v| graph(&v, 0))
}

/// `text[start..]` up to the `}` closing the object that `start` opens, string escapes respected.
fn balanced(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `value` as a graph, unwrapping the keys a metadata blob nests it under — each of which may hold
/// the graph as JSON text rather than an object.
fn graph(value: &Value, depth: u8) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value {
        Value::String(s) => graph(&serde_json::from_str::<Value>(s.trim()).ok()?, depth + 1),
        Value::Object(map) => {
            if is_ui(value) || is_api(map) {
                return Some(value.clone());
            }
            WRAPPERS.iter().filter_map(|k| map.get(*k)).find_map(|v| graph(v, depth + 1))
        }
        _ => None,
    }
}

fn is_ui(value: &Value) -> bool {
    value.get("nodes").and_then(Value::as_array).is_some()
}

fn is_api(map: &serde_json::Map<String, Value>) -> bool {
    map.values().any(|n| n.get("class_type").is_some())
}

/// First occurrence of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let last = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last {
        let off = hay[i..=last].iter().position(|&b| b == needle[0])?;
        let at = i + off;
        if &hay[at..at + needle.len()] == needle {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = r#"{"3":{"class_type":"KSampler","inputs":{"seed":42}},"9":{"class_type":"VHS_VideoCombine","inputs":{"frame_rate":32}}}"#;
    const UI: &str = r#"{"last_node_id":9,"last_link_id":4,"nodes":[{"id":3,"type":"KSamplerAdvanced","widgets_values":[42,8]}],"links":[]}"#;

    /// mp4: an `ilst` tag's `data` box carries 8 zero bytes of version/flags/locale before the text.
    fn mp4(comment: &str) -> Vec<u8> {
        let mut v = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00mdat".to_vec();
        v.extend_from_slice(&[0u8; 64]);
        v.extend_from_slice(b"moovudtametailst\xa9cmtdata");
        v.extend_from_slice(&[0u8; 8]);
        v.extend_from_slice(comment.as_bytes());
        v.extend_from_slice(&[0u8; 16]);
        v
    }

    /// Matroska: `SimpleTag` stores `TagName` then `TagString`, each length-prefixed with binary.
    fn webm(name: &str, value: &str) -> Vec<u8> {
        let mut v = b"\x1a\x45\xdf\xa3".to_vec();
        v.extend_from_slice(&[0u8; 32]);
        v.extend_from_slice(b"\x45\xa3");
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
        v.extend_from_slice(b"\x44\x87");
        v.extend_from_slice(&[0u8; 2]);
        v.extend_from_slice(value.as_bytes());
        v.push(0);
        v
    }

    /// PNG: a `tEXt` chunk is `keyword\0value`, so the NUL bounds the JSON exactly.
    fn png(keyword: &str, value: &str) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0u8; 13]);
        v.extend_from_slice(&[0, 0, 1, 0]);
        v.extend_from_slice(b"tEXt");
        v.extend_from_slice(keyword.as_bytes());
        v.push(0);
        v.extend_from_slice(value.as_bytes());
        v.extend_from_slice(&[0u8; 4]);
        v
    }

    fn nodes_of(json: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(json).unwrap();
        match v.get("nodes").and_then(Value::as_array) {
            Some(arr) => arr
                .iter()
                .filter_map(|n| n.get("type").and_then(Value::as_str).map(String::from))
                .collect(),
            None => v
                .as_object()
                .unwrap()
                .values()
                .filter_map(|n| n.get("class_type").and_then(Value::as_str).map(String::from))
                .collect(),
        }
    }

    /// The shape `VHS_VideoCombine` writes for mp4: one `comment` tag holding both graphs as text.
    #[test]
    fn mp4_comment_yields_the_ui_graph() {
        let comment = format!(
            "{{\"prompt\": {}, \"workflow\": {}}}",
            serde_json::to_string(API).unwrap(),
            serde_json::to_string(UI).unwrap()
        );
        let got = embedded_workflow(&mp4(&comment)).expect("no workflow found");
        assert_eq!(nodes_of(&got), vec!["KSamplerAdvanced"], "UI format must win: it has positions");
    }

    /// A video saved with the API graph alone still opens — that is what the editor round-trips.
    #[test]
    fn mp4_with_only_the_api_prompt_still_resolves() {
        let comment = format!("{{\"prompt\": {}}}", serde_json::to_string(API).unwrap());
        let got = embedded_workflow(&mp4(&comment)).expect("no workflow found");
        assert_eq!(nodes_of(&got), vec!["KSampler", "VHS_VideoCombine"]);
    }

    /// A bare graph with no wrapper (some custom savers) must work too.
    #[test]
    fn a_bare_graph_needs_no_wrapper() {
        assert!(embedded_workflow(&mp4(API)).is_some());
        assert!(embedded_workflow(&mp4(UI)).is_some());
    }

    #[test]
    fn webm_simple_tags_are_found_by_either_key() {
        assert_eq!(nodes_of(&embedded_workflow(&webm("COMMENT", UI)).unwrap()), vec![
            "KSamplerAdvanced"
        ]);
        assert_eq!(nodes_of(&embedded_workflow(&webm("prompt", API)).unwrap()), vec![
            "KSampler",
            "VHS_VideoCombine"
        ]);
    }

    /// Stills work through the same scan, so a viewer with bytes never needs the server.
    #[test]
    fn png_text_chunks_resolve_offline() {
        assert_eq!(nodes_of(&embedded_workflow(&png("prompt", API)).unwrap()), vec![
            "KSampler",
            "VHS_VideoCombine"
        ]);
        assert_eq!(nodes_of(&embedded_workflow(&png("workflow", UI)).unwrap()), vec![
            "KSamplerAdvanced"
        ]);
    }

    /// A pretty-printed graph spans newlines; those must not cut the text run short.
    #[test]
    fn a_pretty_printed_graph_survives_its_newlines() {
        let pretty = serde_json::to_string_pretty(&serde_json::from_str::<Value>(API).unwrap())
            .unwrap();
        assert_eq!(nodes_of(&embedded_workflow(&png("prompt", &pretty)).unwrap()), vec![
            "KSampler",
            "VHS_VideoCombine"
        ]);
    }

    /// A file with no graph must not hand back a stray JSON object that happens to be in it.
    #[test]
    fn media_without_a_workflow_finds_nothing() {
        assert_eq!(embedded_workflow(&mp4("{\"fps\": 32, \"loop\": 0}")), None);
        assert_eq!(embedded_workflow(&[0u8; 4096]), None);
        assert_eq!(embedded_workflow(b""), None);
    }

    /// Truncated / malformed metadata must return None rather than panic on the slicing.
    #[test]
    fn malformed_metadata_is_not_a_panic() {
        assert_eq!(embedded_workflow(b"class_type"), None);
        assert_eq!(embedded_workflow(b"{\"1\":{\"class_type\":\"KSampler\""), None);
        assert_eq!(embedded_workflow(b"widgets_values"), None);
        // Valid JSON, wrong shape, nested past the unwrap limit.
        let deep = "{\"data\":".repeat(10) + "{}" + &"}".repeat(10);
        assert_eq!(embedded_workflow(deep.as_bytes()), None);
    }

    /// The contract the gallery viewer relies on: an mp4's own bytes must yield metadata good enough
    /// to remix, since comfy-gate answers 415 for anything but a PNG.
    #[test]
    fn a_wan_video_yields_remixable_metadata() {
        let ui = r#"{"last_node_id":9,"nodes":[
            {"id":1,"type":"UNETLoader","widgets_values":["Wan/wan2.2_i2v_high_noise_14B.safetensors","default"]},
            {"id":2,"type":"UNETLoader","widgets_values":["Wan/wan2.2_i2v_low_noise_14B.safetensors","default"]},
            {"id":3,"type":"CLIPLoader","widgets_values":["umt5_xxl_fp8.safetensors","wan","cpu"]},
            {"id":4,"type":"VAELoader","widgets_values":["wan_2.1_vae.safetensors"]},
            {"id":5,"type":"CLIPTextEncode","widgets_values":["a cat turning its head"]},
            {"id":6,"type":"LoraLoaderModelOnly","widgets_values":["Wan/lightx2v_high.safetensors",0.7]},
            {"id":7,"type":"KSamplerAdvanced","widgets_values":["enable",99,"fixed",8,2.5,"euler","simple",0,4,"enable"],
             "inputs":[{"name":"positive","link":1}]},
            {"id":8,"type":"VHS_VideoCombine","widgets_values":{"frame_rate":32,"format":"video/h264-mp4"}}
        ],"links":[[1,5,0,7,1,"CONDITIONING"]]}"#;
        let comment = format!("{{\"workflow\": {}}}", serde_json::to_string(ui).unwrap());
        let json = embedded_workflow(&mp4(&comment)).expect("no workflow found");

        let meta = crate::gallery::parse_workflow_meta_for(&json, Some("wan_00042.mp4"));
        assert!(!meta.is_empty(), "an empty scrape leaves Remix dead for every video");
        assert_eq!(meta.unet.as_deref(), Some("Wan/wan2.2_i2v_high_noise_14B.safetensors"));
        assert_eq!(meta.clips, vec!["umt5_xxl_fp8.safetensors"]);
        assert_eq!(meta.clip_type.as_deref(), Some("wan"));
        assert_eq!(meta.vae.as_deref(), Some("wan_2.1_vae.safetensors"));
        assert_eq!(meta.positive.as_deref(), Some("a cat turning its head"));
        assert_eq!(meta.steps, Some(8), "KSamplerAdvanced slots, not KSampler's");
        assert_eq!(meta.cfg, Some(2.5));
        assert_eq!(meta.sampler.as_deref(), Some("euler"));
        assert_eq!(meta.scheduler.as_deref(), Some("simple"));
        assert_eq!(meta.loras.len(), 1);
        assert!(meta.loras[0].model_only);
    }

    /// Only the newest write matters little — but a graph late in a big file must still be found.
    #[test]
    fn a_graph_after_megabytes_of_frames_is_found() {
        let mut v = vec![0x5au8; 3 << 20];
        v.extend_from_slice(&png("prompt", API));
        assert!(embedded_workflow(&v).is_some());
    }
}
