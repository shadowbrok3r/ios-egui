//! Prompt/model linting: warnings and one-assignment fixes for the Create tab. Pure.

use crate::tags;
use crate::types::{
    ActiveLora, CheckpointEntry, LoraEntry, Params, append_negatives, file_basename,
    merge_triggers, split_triggers,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Info,
}

/// A whole-field replacement string, so applying a fix is a single assignment.
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    SetPositive(String),
    SetNegative(String),
    SetLoraTriggers(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LintIssue {
    pub severity: Severity,
    pub msg: String,
    pub fix: Option<Fix>,
}

/// Comparison key for tokens: trimmed, ASCII-lowercased, underscores folded to spaces.
fn fold(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', " ")
}

/// Whether `needle` appears as a chip token in `haystack` (weights/case/underscores ignored).
fn present(haystack: &str, needle: &str) -> bool {
    let n = fold(needle);
    if n.is_empty() {
        return true;
    }
    tags::parse_chips(haystack).iter().any(|c| fold(&c.tag) == n)
}

/// Trigger tokens a LoRA wants (catalog union injected) that are absent from `combined`.
fn missing_lora_triggers(al: &ActiveLora, entry: Option<&LoraEntry>, combined: &str) -> Vec<String> {
    let mut want: Vec<String> = Vec::new();
    if let Some(e) = entry {
        for t in &e.trigger_words {
            let t = t.trim();
            if !t.is_empty() {
                want.push(t.to_string());
            }
        }
    }
    want.extend(split_triggers(&al.injected));
    let mut seen = std::collections::HashSet::new();
    want.into_iter()
        .filter(|t| seen.insert(fold(t)))
        .filter(|t| !present(combined, t))
        .collect()
}

/// (base_model substrings, want any-of, positive prefix, negative adds) family quality table.
const FAMILY_QUALITY: &[(&[&str], &[&str], &str, &[&str])] = &[
    (&["pony"], &["score_9"], "score_9, score_8_up, score_7_up, ", &["score_4", "score_5", "score_6"]),
    (
        &["illustrious", "noobai"],
        &["masterpiece", "best quality"],
        "masterpiece, best quality, ",
        &["worst quality", "low quality"],
    ),
    (
        &["sd1", "sd 1.5", "sd15"],
        &["masterpiece", "best quality"],
        "masterpiece, best quality, ",
        &["worst quality", "low quality"],
    ),
];

/// The first family-quality row whose base substrings match the checkpoint, if any.
fn family_row(ckpt: &CheckpointEntry) -> Option<&'static (&'static [&'static str], &'static [&'static str], &'static str, &'static [&'static str])> {
    let hay = format!(
        "{} {}",
        ckpt.base_model.as_deref().unwrap_or("").to_ascii_lowercase(),
        ckpt.base_model_type.as_deref().unwrap_or("").to_ascii_lowercase(),
    );
    FAMILY_QUALITY.iter().find(|(bases, _, _, _)| bases.iter().any(|b| hay.contains(b)))
}

/// Paren depth of `text`, ignoring escaped `\(` / `\)`.
fn paren_balance(text: &str) -> i32 {
    let mut depth = 0i32;
    let mut escaped = false;
    for &c in text.as_bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            b'\\' => escaped = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// A `N<noun>` count tag split into (number, lowercase noun), else `None`.
fn parse_count(tag: &str) -> Option<(u32, String)> {
    let t = tag.trim();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &t[digits.len()..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() || rest.contains(' ') {
        return None;
    }
    Some((digits.parse().ok()?, rest.to_ascii_lowercase()))
}

/// Pairs of count tags that share a noun stem but disagree on the number.
fn count_conflicts(text: &str) -> Vec<(String, String)> {
    let mut by_stem: std::collections::HashMap<String, (u32, String)> =
        std::collections::HashMap::new();
    let mut conflicts = Vec::new();
    for chip in tags::parse_chips(text) {
        if let Some((num, noun)) = parse_count(&chip.tag) {
            let stem = noun.trim_end_matches('s').to_string();
            match by_stem.get(&stem) {
                Some((n, first)) if *n != num => {
                    conflicts.push((first.clone(), chip.tag.clone()));
                }
                Some(_) => {}
                None => {
                    by_stem.insert(stem, (num, chip.tag.clone()));
                }
            }
        }
    }
    conflicts
}

/// Lint the Create-tab params against the selected checkpoint and its active LoRA catalog data.
pub fn lint(
    params: &Params,
    ckpt: Option<&CheckpointEntry>,
    loras: &[(&ActiveLora, Option<&LoraEntry>)],
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let combined = params.combined_positive();

    // 1. Missing LoRA triggers.
    for (al, entry) in loras {
        let missing = missing_lora_triggers(al, *entry, &combined);
        if missing.is_empty() {
            continue;
        }
        let name = entry.map(|e| e.display_name()).unwrap_or_else(|| file_basename(&al.file));
        let mut lt = params.active_lora_triggers().to_string();
        merge_triggers(&mut lt, &missing.join(", "), &params.positive);
        issues.push(LintIssue {
            severity: Severity::Warn,
            msg: format!("LoRA {name} is missing trigger(s): {}", missing.join(", ")),
            fix: Some(Fix::SetLoraTriggers(lt)),
        });
    }

    // 2. Family quality block.
    if let Some(ckpt) = ckpt {
        if let Some((_, want, prefix, negatives)) = family_row(ckpt) {
            if !want.iter().any(|w| present(&combined, w)) {
                issues.push(LintIssue {
                    severity: Severity::Info,
                    msg: format!("This model expects quality tags (e.g. {})", want.join(", ")),
                    fix: Some(Fix::SetPositive(format!("{prefix}{}", params.positive))),
                });
            }
            if !negatives.iter().any(|n| present(&params.negative, n)) {
                let mut neg = params.negative.clone();
                for n in *negatives {
                    append_negatives(&mut neg, n);
                }
                issues.push(LintIssue {
                    severity: Severity::Info,
                    msg: format!("This model expects quality negatives ({})", negatives.join(", ")),
                    fix: Some(Fix::SetNegative(neg)),
                });
            }
        }
    }

    // 3. Duplicate tags in the positive prompt — internal repeats, or a tag the LoRA-trigger
    // field already carries (the trigger field is prepended, so a shared tag encodes twice).
    let deduped = tags::dedupe_against(&params.positive, params.active_lora_triggers());
    if deduped != params.positive {
        issues.push(LintIssue {
            severity: Severity::Info,
            msg: "Duplicate tags in the positive prompt".to_string(),
            fix: Some(Fix::SetPositive(deduped)),
        });
    }

    // 4. Unbalanced attention parens.
    let balance = paren_balance(&params.positive);
    if balance > 0 {
        let fixed = format!("{}{}", params.positive, ")".repeat(balance as usize));
        issues.push(LintIssue {
            severity: Severity::Warn,
            msg: format!("Unbalanced parens: {balance} unclosed '('"),
            fix: Some(Fix::SetPositive(fixed)),
        });
    } else if balance < 0 {
        let fixed = format!("{}{}", "(".repeat((-balance) as usize), params.positive);
        issues.push(LintIssue {
            severity: Severity::Warn,
            msg: format!("Unbalanced parens: {} extra ')'", -balance),
            fix: Some(Fix::SetPositive(fixed)),
        });
    }

    // 5. Conflicting subject counts.
    for (a, b) in count_conflicts(&params.positive) {
        issues.push(LintIssue {
            severity: Severity::Warn,
            msg: format!("Conflicting subject counts: {a} vs {b}"),
            fix: None,
        });
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ckpt(base: &str) -> CheckpointEntry {
        serde_json::from_value(json!({"file": "m.safetensors", "base_model": base})).unwrap()
    }
    fn lora_entry(file: &str, triggers: &[&str]) -> LoraEntry {
        serde_json::from_value(json!({"file": file, "trigger_words": triggers})).unwrap()
    }
    fn active(file: &str) -> ActiveLora {
        serde_json::from_value(json!({"file": file, "strength_model": 1.0, "strength_clip": 1.0}))
            .unwrap()
    }

    #[test]
    fn missing_lora_triggers_are_reported_and_fixed() {
        let params = Params { positive: "a cat".into(), ..Default::default() };
        let al = active("style.safetensors");
        let entry = lora_entry("style.safetensors", &["style_tag", "glow"]);
        let issues = lint(&params, None, &[(&al, Some(&entry))]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warn);
        match &issues[0].fix {
            Some(Fix::SetLoraTriggers(s)) => assert_eq!(s, "style_tag, glow"),
            other => panic!("wrong fix: {other:?}"),
        }
        // Once the trigger is present the check is silent (folding underscores/case).
        let params2 = Params { positive: "Style Tag, glow, a cat".into(), ..Default::default() };
        assert!(lint(&params2, None, &[(&al, Some(&entry))]).is_empty());
    }

    #[test]
    fn pony_family_wants_score_tags_and_negatives() {
        let params = Params { positive: "a knight".into(), negative: String::new(), ..Default::default() };
        let issues = lint(&params, Some(&ckpt("Pony")), &[]);
        let pos = issues.iter().find_map(|i| match &i.fix {
            Some(Fix::SetPositive(s)) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(pos.as_deref(), Some("score_9, score_8_up, score_7_up, a knight"));
        let neg = issues.iter().find_map(|i| match &i.fix {
            Some(Fix::SetNegative(s)) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(neg.as_deref(), Some("score_4, score_5, score_6"));
        // Satisfied prompt fires neither.
        let ok = Params {
            positive: "score_9, a knight".into(),
            negative: "score_4, score_5, score_6".into(),
            ..Default::default()
        };
        assert!(lint(&ok, Some(&ckpt("Pony")), &[]).is_empty());
    }

    #[test]
    fn illustrious_family_wants_masterpiece() {
        let params = Params { positive: "a girl".into(), negative: String::new(), ..Default::default() };
        let issues = lint(&params, Some(&ckpt("Illustrious")), &[]);
        assert!(issues.iter().any(|i| matches!(&i.fix,
            Some(Fix::SetPositive(s)) if s == "masterpiece, best quality, a girl")));
    }

    #[test]
    fn duplicate_tags_flagged_with_dedupe_fix() {
        let params = Params { positive: "sky, tree, sky".into(), ..Default::default() };
        let issues = lint(&params, None, &[]);
        assert!(issues.iter().any(|i| matches!(&i.fix,
            Some(Fix::SetPositive(s)) if s == "sky, tree")));
    }

    #[test]
    fn positive_tags_already_in_lora_triggers_are_flagged() {
        // "1girl" and "long hair" (fold of "long_hair") both duplicate the trigger field.
        let params = Params {
            positive: "1girl, standing, long_hair".into(),
            lora_triggers: "1girl, long hair".into(),
            ..Default::default()
        };
        let fix = lint(&params, None, &[]).into_iter().find_map(|i| match i.fix {
            Some(Fix::SetPositive(s)) => Some(s),
            _ => None,
        });
        assert_eq!(fix.as_deref(), Some("standing"));
        // No overlap → no duplicate issue.
        let clean = Params {
            positive: "standing".into(),
            lora_triggers: "1girl".into(),
            ..Default::default()
        };
        assert!(!lint(&clean, None, &[]).iter().any(|i| i.msg.contains("Duplicate")));
    }

    #[test]
    fn unbalanced_parens_get_closers_appended() {
        let params = Params { positive: "((a cat".into(), ..Default::default() };
        let issues = lint(&params, None, &[]);
        assert!(issues.iter().any(|i| i.severity == Severity::Warn
            && matches!(&i.fix, Some(Fix::SetPositive(s)) if s == "((a cat))")));
        // Escaped parens are not counted.
        let ok = Params { positive: "\\(a cat\\)".into(), ..Default::default() };
        assert!(!lint(&ok, None, &[]).iter().any(|i| i.msg.contains("Unbalanced")));
    }

    #[test]
    fn conflicting_subject_counts_warn_without_fix() {
        let params = Params { positive: "1girl, 2girls, garden".into(), ..Default::default() };
        let issues = lint(&params, None, &[]);
        let c = issues.iter().find(|i| i.msg.contains("Conflicting subject counts")).unwrap();
        assert_eq!(c.severity, Severity::Warn);
        assert!(c.fix.is_none());
        // Same count, no conflict.
        let ok = Params { positive: "1girl, 1boy".into(), ..Default::default() };
        assert!(!lint(&ok, None, &[]).iter().any(|i| i.msg.contains("Conflicting")));
    }
}
