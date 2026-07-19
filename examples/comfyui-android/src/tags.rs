//! Danbooru tag dictionary plus prompt-text token/chip helpers. Pure: no egui/android deps.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Range;
use std::sync::OnceLock;

/// One dictionary entry. Alias rows carry `canonical` = the index of the tag they redirect to.
pub struct TagEntry {
    pub name: String,
    pub category: u8,
    pub count: u32,
    pub canonical: Option<u32>,
}

impl TagEntry {
    /// Prompt insertion form: underscores rendered as spaces.
    pub fn insert_text(&self) -> String {
        self.name.replace('_', " ")
    }
}

/// Name-sorted (case-insensitive, spaces-folded) tag dictionary backing prefix suggestions.
pub struct TagDict {
    entries: Vec<TagEntry>,
}

/// Comparison/lookup key: trimmed, ASCII-lowercased, underscores folded to spaces.
fn fold(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', " ")
}

/// Split one CSV line into fields, honoring RFC-4180 double-quoted fields (`""` = literal quote).
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => fields.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
    }
    fields.push(cur);
    fields
}

impl TagDict {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parse a gzip-compressed `name,category,count,"alias,alias"` CSV into a name-sorted dict.
    pub fn parse_csv_gz(bytes: &[u8]) -> Result<TagDict, String> {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;

        let mut entries: Vec<TagEntry> = Vec::new();
        let mut canon: Vec<Option<String>> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let fields = split_csv(line);
            if fields.len() < 3 {
                continue;
            }
            let name = fields[0].trim().to_string();
            if name.is_empty() {
                continue;
            }
            let category = fields[1].trim().parse().unwrap_or(0u8);
            let count = fields[2].trim().parse().unwrap_or(0u32);
            entries.push(TagEntry { name: name.clone(), category, count, canonical: None });
            canon.push(None);
            if let Some(aliases) = fields.get(3) {
                for a in aliases.split(',').map(str::trim).filter(|a| !a.is_empty()) {
                    entries.push(TagEntry {
                        name: a.to_string(),
                        category,
                        count,
                        canonical: Some(0),
                    });
                    canon.push(Some(name.clone()));
                }
            }
        }

        let mut zipped: Vec<(TagEntry, Option<String>)> =
            entries.into_iter().zip(canon).collect();
        zipped.sort_by_cached_key(|(e, _)| fold(&e.name));
        let (mut entries, canon): (Vec<TagEntry>, Vec<Option<String>>) =
            zipped.into_iter().unzip();

        let mut idx_by_name: HashMap<String, u32> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if e.canonical.is_none() {
                idx_by_name.entry(e.name.clone()).or_insert(i as u32);
            }
        }
        for (i, cn) in canon.iter().enumerate() {
            if let Some(name) = cn {
                entries[i].canonical = idx_by_name.get(name).copied();
            }
        }
        Ok(TagDict { entries })
    }

    /// The bundled Danbooru dictionary, parsed once and cached (~50ms on first touch).
    pub fn bundled() -> &'static TagDict {
        static BUNDLED: OnceLock<TagDict> = OnceLock::new();
        BUNDLED.get_or_init(|| {
            // Danbooru tag list from DominikDoom/a1111-sd-webui-tagcomplete (tags/danbooru.csv), trimmed.
            const BYTES: &[u8] = include_bytes!("../assets/tags/danbooru.csv.gz");
            TagDict::parse_csv_gz(BYTES).unwrap_or_else(|_| TagDict { entries: Vec::new() })
        })
    }

    /// Prefix matches ranked by count desc; aliases resolve to (and dedupe against) their canonical.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<&TagEntry> {
        let q = fold(prefix);
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let lo = self.entries.partition_point(|e| fold(&e.name) < q);
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<&TagEntry> = Vec::new();
        for e in &self.entries[lo..] {
            if !fold(&e.name).starts_with(&q) {
                break;
            }
            let canonical = match e.canonical {
                Some(ci) => &self.entries[ci as usize],
                None => e,
            };
            if seen.insert(fold(&canonical.name)) {
                out.push(canonical);
            }
        }
        out.sort_by(|a, b| b.count.cmp(&a.count));
        out.truncate(limit);
        out
    }
}

/// Convenience: [`TagDict::bundled`] then [`TagDict::suggest`].
pub fn suggest(prefix: &str, limit: usize) -> Vec<&'static TagEntry> {
    TagDict::bundled().suggest(prefix, limit)
}

/// The partial tag under `cursor_byte`: from the last `,` / newline / `(` back-scan to the cursor,
/// trimmed and stripped of any trailing `:weight`. Multibyte-safe; returns its byte range and slice.
pub fn token_at(text: &str, cursor_byte: usize) -> (Range<usize>, &str) {
    let mut cursor = cursor_byte.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for i in (0..cursor).rev() {
        if matches!(bytes[i], b',' | b'\n' | b'(') {
            start = i + 1;
            break;
        }
    }
    let slice = &text[start..cursor];
    // Strip a trailing `:weight` (numeric suffix), leaving the tag name.
    let core = match slice.rfind(':') {
        Some(pos) => {
            let suffix = slice[pos + 1..].trim();
            let numeric = !suffix.is_empty()
                && suffix.chars().all(|c| c.is_ascii_digit() || c == '.');
            if numeric { &slice[..pos] } else { slice }
        }
        None => slice,
    };
    let lead = core.len() - core.trim_start().len();
    let tok = core.trim();
    let tok_start = start + lead;
    let tok_end = tok_start + tok.len();
    (tok_start..tok_end, &text[tok_start..tok_end])
}

/// Replace `range` with `tag, ` and return the new text plus the cursor byte after the insertion.
pub fn accept_suggestion(text: &str, range: Range<usize>, tag: &str) -> (String, usize) {
    let insert = format!("{tag}, ");
    let cursor = range.start + insert.len();
    let mut out = String::with_capacity(text.len() - (range.end - range.start) + insert.len());
    out.push_str(&text[..range.start]);
    out.push_str(&insert);
    out.push_str(&text[range.end..]);
    (out, cursor)
}

/// One comma-separated prompt token with its byte span, peeled tag text, and effective weight.
pub struct Chip {
    pub range: Range<usize>,
    pub tag: String,
    pub weight: f32,
}

/// Top-level (paren-depth-zero, unescaped) comma-separated byte ranges over `text`.
fn top_level_segments(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut segs = Vec::new();
    let mut depth = 0i32;
    let mut escaped = false;
    let mut start = 0usize;
    for i in 0..b.len() {
        if escaped {
            escaped = false;
            continue;
        }
        match b[i] {
            b'\\' => escaped = true,
            b'(' => depth += 1,
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b',' if depth == 0 => {
                segs.push(start..i);
                start = i + 1;
            }
            _ => {}
        }
    }
    segs.push(start..b.len());
    segs
}

/// Whether `s` opens with `(` and that paren closes exactly at the final byte (honoring `\(`).
fn wraps_in_parens(s: &str) -> bool {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') {
        return false;
    }
    let mut depth = 0i32;
    let mut escaped = false;
    for (i, &c) in b.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            b'\\' => escaped = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i == b.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// Peel attention parens off a trimmed segment into (tag text, effective weight).
/// Each bare `(...)` layer multiplies by 1.1; a `(tag:W)` layer multiplies by W.
fn parse_segment(seg: &str) -> (String, f32) {
    let mut s = seg.trim();
    let mut weight = 1.0f32;
    while wraps_in_parens(s) {
        let inner = s[1..s.len() - 1].trim();
        if let Some(pos) = inner.rfind(':') {
            if let Ok(w) = inner[pos + 1..].trim().parse::<f32>() {
                weight *= w;
                s = inner[..pos].trim();
                continue;
            }
        }
        weight *= 1.1;
        s = inner;
    }
    (s.to_string(), weight)
}

/// Parse `text` into top-level chips: split on unescaped depth-zero commas, peel `(tag:W)` weights.
pub fn parse_chips(text: &str) -> Vec<Chip> {
    let mut chips = Vec::new();
    for seg in top_level_segments(text) {
        let raw = &text[seg.clone()];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lead = raw.len() - raw.trim_start().len();
        let start = seg.start + lead;
        let end = start + trimmed.len();
        let (tag, weight) = parse_segment(trimmed);
        chips.push(Chip { range: start..end, tag, weight });
    }
    chips
}

fn splice(text: &str, range: Range<usize>, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len() - (range.end - range.start) + replacement.len());
    out.push_str(&text[..range.start]);
    out.push_str(replacement);
    out.push_str(&text[range.end..]);
    out
}

/// Weight formatted at 2 decimals with trailing zeros / dot trimmed.
fn trim_weight(w: f32) -> String {
    let s = format!("{w:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// `tag` at weight `w`: bare tag when within 0.005 of 1.0, else `(tag:W)`.
fn render_chip(tag: &str, w: f32) -> String {
    if (w - 1.0).abs() < 0.005 {
        tag.to_string()
    } else {
        format!("({tag}:{})", trim_weight(w))
    }
}

/// Snap a weight to the nearest 0.05 step.
fn snap_step(w: f32) -> f32 {
    (w / 0.05).round() * 0.05
}

/// Adjust chip `idx`'s weight by `delta` (snapped to 0.05, clamped 0.5..=2.0) and rewrite the text.
pub fn bump_weight(text: &str, idx: usize, delta: f32) -> String {
    let chips = parse_chips(text);
    let Some(chip) = chips.get(idx) else {
        return text.to_string();
    };
    let new_w = snap_step(chip.weight + delta).clamp(0.5, 2.0);
    splice(text, chip.range.clone(), &render_chip(&chip.tag, new_w))
}

/// Remove chip `idx` along with one adjoining `, ` (trailing preferred, else leading).
pub fn remove_chip(text: &str, idx: usize) -> String {
    let chips = parse_chips(text);
    let Some(chip) = chips.get(idx) else {
        return text.to_string();
    };
    let b = text.as_bytes();
    let mut lo = chip.range.start;
    let mut hi = chip.range.end;
    let mut j = hi;
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    if j < b.len() && b[j] == b',' {
        j += 1;
        while j < b.len() && b[j] == b' ' {
            j += 1;
        }
        hi = j;
    } else {
        let mut k = lo;
        while k > 0 && b[k - 1] == b' ' {
            k -= 1;
        }
        if k > 0 && b[k - 1] == b',' {
            k -= 1;
            while k > 0 && b[k - 1] == b' ' {
                k -= 1;
            }
            lo = k;
        }
    }
    let mut out = String::with_capacity(text.len() - (hi - lo));
    out.push_str(&text[..lo]);
    out.push_str(&text[hi..]);
    out
}

/// Swap chip `idx` with its `dir` neighbor (-1 left, +1 right); separators stay put.
pub fn move_chip(text: &str, idx: usize, dir: i8) -> String {
    let chips = parse_chips(text);
    let target = idx as isize + dir as isize;
    if idx >= chips.len() || target < 0 || target as usize >= chips.len() {
        return text.to_string();
    }
    let j = target as usize;
    let (a, b) = if idx < j { (idx, j) } else { (j, idx) };
    let ra = chips[a].range.clone();
    let rb = chips[b].range.clone();
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..ra.start]);
    out.push_str(&text[rb.clone()]);
    out.push_str(&text[ra.end..rb.start]);
    out.push_str(&text[ra.clone()]);
    out.push_str(&text[rb.end..]);
    out
}

/// Drop later duplicate tags (case/underscore-insensitive), keeping the first survivor verbatim.
pub fn dedupe(text: &str) -> String {
    let chips = parse_chips(text);
    let mut seen: HashSet<String> = HashSet::new();
    let mut keep: Vec<Range<usize>> = Vec::new();
    for chip in &chips {
        if seen.insert(fold(&chip.tag)) {
            keep.push(chip.range.clone());
        }
    }
    if keep.len() == chips.len() {
        return text.to_string();
    }
    keep.iter()
        .map(|r| &text[r.clone()])
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    fn gz(s: &str) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(s.as_bytes()).unwrap();
        e.finish().unwrap()
    }

    const FIXTURE: &str = "banana,0,300,\n\
        apricot,0,500,aprico\n\
        apple,0,100,\n\
        long_hair,0,400,longhair\n\
        breasts,0,3000,\"boobs,tits\"\n";

    #[test]
    fn token_at_handles_plain_weight_and_multibyte() {
        let text = "a cat, long h";
        let (r, tok) = token_at(text, text.len());
        assert_eq!(tok, "long h");
        assert_eq!(&text[r], "long h");

        let wt = "(long hair:1.2";
        let (r, tok) = token_at(wt, wt.len());
        assert_eq!(tok, "long hair");
        assert_eq!(&wt[r], "long hair");

        // The token itself carries multibyte bytes; offsets must land on char boundaries.
        let mb = "a, naïve café";
        let (r, tok) = token_at(mb, mb.len());
        assert_eq!(tok, "naïve café");
        assert_eq!(&mb[r.clone()], "naïve café");
        let (out, cur) = accept_suggestion(mb, r, "naive cafe");
        assert_eq!(out, "a, naive cafe, ");
        assert_eq!(cur, out.len());
    }

    #[test]
    fn accept_suggestion_replaces_partial() {
        let text = "a cat, long h";
        let (r, _) = token_at(text, text.len());
        let (out, cur) = accept_suggestion(text, r, "long hair");
        assert_eq!(out, "a cat, long hair, ");
        assert_eq!(cur, out.len());
    }

    #[test]
    fn parse_chips_reads_weights_and_nesting() {
        let text = "1girl, (long hair:1.2), ((detailed)), \\(smile\\)";
        let chips = parse_chips(text);
        assert_eq!(chips.len(), 4);
        assert_eq!(chips[0].tag, "1girl");
        assert!((chips[0].weight - 1.0).abs() < 1e-4);
        assert_eq!(chips[1].tag, "long hair");
        assert!((chips[1].weight - 1.2).abs() < 1e-4);
        assert_eq!(chips[2].tag, "detailed");
        assert!((chips[2].weight - 1.21).abs() < 1e-4);
        // Escaped parens stay literal text, not a weight wrapper.
        assert_eq!(chips[3].tag, "\\(smile\\)");
        assert!((chips[3].weight - 1.0).abs() < 1e-4);
    }

    #[test]
    fn rewrites_round_trip_through_parse() {
        let text = "1girl, long hair, detailed";
        // bump: +0.2 -> 1.2 rendered as (tag:1.2), re-parse agrees.
        let bumped = bump_weight(text, 1, 0.2);
        assert_eq!(bumped, "1girl, (long hair:1.2), detailed");
        let re = parse_chips(&bumped);
        assert_eq!(re[1].tag, "long hair");
        assert!((re[1].weight - 1.2).abs() < 1e-4);
        // bumping back to 1.0 strips the wrapper entirely.
        let restored = bump_weight(&bumped, 1, -0.2);
        assert_eq!(restored, text);

        // remove eats the adjoining comma+space.
        assert_eq!(remove_chip(text, 1), "1girl, detailed");
        assert_eq!(remove_chip(text, 0), "long hair, detailed");
        assert_eq!(remove_chip(text, 2), "1girl, long hair");

        // move swaps neighbors, preserving separators.
        let moved = move_chip(text, 0, 1);
        assert_eq!(moved, "long hair, 1girl, detailed");
        assert_eq!(parse_chips(&moved)[0].tag, "long hair");

        // dedupe keeps the first survivor's formatting.
        assert_eq!(
            dedupe("(masterpiece:1.2), long_hair, masterpiece, LONG HAIR"),
            "(masterpiece:1.2), long_hair"
        );
    }

    #[test]
    fn suggest_ranks_by_count_and_folds_aliases() {
        let dict = TagDict::parse_csv_gz(&gz(FIXTURE)).unwrap();
        // "ap" matches apple(100), apricot(500) and alias aprico->apricot; ranked by count desc.
        let names: Vec<&str> = dict.suggest("ap", 5).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["apricot", "apple"]);

        // alias folds to its canonical and dedupes against the main entry.
        let apr = dict.suggest("apr", 5);
        assert_eq!(apr.len(), 1);
        assert_eq!(apr[0].name, "apricot");

        // underscore<->space folding in the query; insertion form uses spaces.
        let lh = dict.suggest("long h", 5);
        assert_eq!(lh.len(), 1);
        assert_eq!(lh[0].name, "long_hair");
        assert_eq!(lh[0].insert_text(), "long hair");

        // alias "boobs" resolves to canonical "breasts".
        let b = dict.suggest("boo", 5);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "breasts");
    }

    #[test]
    fn bundled_parses_a_large_dictionary() {
        let dict = TagDict::bundled();
        assert!(dict.len() > 10_000, "bundled dict too small: {}", dict.len());
        assert!(!dict.suggest("1girl", 5).is_empty());
    }
}
