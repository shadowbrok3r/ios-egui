//! Builtin prompt templates and the Qwen2.5 chat-format assembly. Pure string logic,
//! host-testable; the model just consumes what [`build_prompt`] emits.

/// Chat-turn delimiters used by the Qwen2.5 template.
pub const IM_START: &str = "<|im_start|>";
pub const IM_END: &str = "<|im_end|>";

/// System prompt: danbooru tags -> natural-language Wan video motion prose.
pub const SYS_TAGS_TO_VIDEO: &str = "\
You convert danbooru-style tags into one short cinematic video motion description for a \
text-to-video model. Write flowing natural language, present tense, describing subject, \
setting, and especially motion and camera movement. Keep it under 80 words. Output only the \
description, no tags, no preamble, no quotes.";

/// System prompt: natural-language prose -> comma-separated danbooru tags.
pub const SYS_PROSE_TO_TAGS: &str = "\
You convert a natural-language image description into danbooru-style tags for an anime image \
model. Output a single comma-separated line of lowercase tags with underscores between words \
of a tag. Order: subject count, then subject, then appearance, clothing, pose, setting, \
lighting. Output only the tags, no prose, no preamble.";

/// System prompt: rewrite a prompt into the Pony Diffusion dialect (score_ quality block).
/// Each family prompt carries one worked example: the 0.5B model holds a format far better
/// than it follows a description of one, and without it a prose input can send it parroting
/// the instructions instead of rewriting.
pub const SYS_FAMILY_TO_PONY: &str = "\
You rewrite an anime image prompt into the Pony Diffusion dialect. Begin with the quality \
block 'score_9, score_8_up, score_7_up, score_6_up, score_5_up, score_4_up' then the source \
content as comma-separated tags. Remove any masterpiece/best-quality style tags. Never repeat \
a tag. Output only the rewritten prompt.\n\
Example input: a girl in silver armor standing on a cliff at sunset\n\
Example output: score_9, score_8_up, score_7_up, score_6_up, score_5_up, score_4_up, 1girl, \
solo, silver armor, standing, cliff, sunset, dramatic lighting";

/// System prompt: rewrite a prompt into the Illustrious/NoobAI dialect (masterpiece block).
pub const SYS_FAMILY_TO_ILLUSTRIOUS: &str = "\
You rewrite an anime image prompt into the Illustrious/NoobAI dialect. Begin with the quality \
block 'masterpiece, best quality, newest, absurdres, highres' then the source content as \
comma-separated tags. Remove any score_ quality tags. Never repeat a tag; never write model \
or dialect names into the prompt. Output only the rewritten prompt.\n\
Example input: a girl in silver armor standing on a cliff at sunset\n\
Example output: masterpiece, best quality, newest, absurdres, highres, 1girl, solo, silver \
armor, standing, cliff, sunset, dramatic lighting";

/// System prompt: rewrite a prompt for the Anima (Qwen3-encoder) DiT — hybrid prose + tags, no
/// quality block.
pub const SYS_FAMILY_TO_ANIMA: &str = "\
You rewrite an anime image prompt for the Anima model, whose Qwen3 text encoder reads a hybrid \
of natural language plus booru tags. Remove any Pony score_ tags and any Illustrious/NoobAI \
quality tags (masterpiece, best quality, newest, absurdres, highres); do not add any quality \
block. Keep the character, subject, and content tags. Begin with one short natural-language \
sentence describing the scene, then the kept content as comma-separated tags. Never repeat a \
tag. Output only the rewritten prompt.\n\
Example input: masterpiece, best quality, 1girl, solo, silver armor, cliff, sunset\n\
Example output: A lone girl in silver armor stands on a cliff at sunset. 1girl, solo, silver \
armor, cliff, sunset";

/// Which rewrite the model should perform; maps to a system prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteKind {
    TagsToVideo,
    ProseToTags,
    ToPony,
    ToIllustrious,
    ToAnima,
}

impl RewriteKind {
    /// The system prompt for this rewrite.
    pub fn system(self) -> &'static str {
        match self {
            RewriteKind::TagsToVideo => SYS_TAGS_TO_VIDEO,
            RewriteKind::ProseToTags => SYS_PROSE_TO_TAGS,
            RewriteKind::ToPony => SYS_FAMILY_TO_PONY,
            RewriteKind::ToIllustrious => SYS_FAMILY_TO_ILLUSTRIOUS,
            RewriteKind::ToAnima => SYS_FAMILY_TO_ANIMA,
        }
    }

    /// Short menu label.
    pub fn label(self) -> &'static str {
        match self {
            RewriteKind::TagsToVideo => "To video prose",
            RewriteKind::ProseToTags => "To tags",
            RewriteKind::ToPony => "To Pony",
            RewriteKind::ToIllustrious => "To Illustrious",
            RewriteKind::ToAnima => "To Anima",
        }
    }

    /// True when the output is tag-shaped and should be run through
    /// [`dedupe_comma_segments`] (video prose is the one free-text output).
    pub fn dedupes_tags(self) -> bool {
        !matches!(self, RewriteKind::TagsToVideo)
    }
}

/// Drop exact-duplicate comma segments (case-insensitive), keeping first occurrences. A tag
/// never needs to appear twice in a prompt, and a degenerate decode's favourite failure is
/// repeating one — this makes even a cut-off loop come out clean.
pub fn dedupe_comma_segments(s: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<&str> = Vec::new();
    for raw in s.split(',') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_ascii_lowercase()) {
            kept.push(t);
        }
    }
    kept.join(", ")
}

/// Assemble the Qwen2.5 chat prompt: one system turn, one user turn, open assistant turn.
pub fn build_prompt(system: &str, user: &str) -> String {
    format!(
        "{IM_START}system\n{system}{IM_END}\n{IM_START}user\n{user}{IM_END}\n{IM_START}assistant\n"
    )
}

/// A prompt-quality dialect keyed to a model family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptFamily {
    Pony,
    Illustrious,
}

/// Pony Diffusion's quality-prefix tags.
pub const PONY_QUALITY_TAGS: [&str; 6] =
    ["score_9", "score_8_up", "score_7_up", "score_6_up", "score_5_up", "score_4_up"];

/// Illustrious/NoobAI's quality-prefix tags.
pub const ILLUSTRIOUS_QUALITY_TAGS: [&str; 5] =
    ["masterpiece", "best quality", "newest", "absurdres", "highres"];

/// Pony quality block, comma-joined.
pub const PONY_QUALITY_BLOCK: &str =
    "score_9, score_8_up, score_7_up, score_6_up, score_5_up, score_4_up";

/// Illustrious quality block, comma-joined.
pub const ILLUSTRIOUS_QUALITY_BLOCK: &str =
    "masterpiece, best quality, newest, absurdres, highres";

impl PromptFamily {
    /// The comma-joined quality block this family prepends.
    pub fn quality_block(self) -> &'static str {
        match self {
            PromptFamily::Pony => PONY_QUALITY_BLOCK,
            PromptFamily::Illustrious => ILLUSTRIOUS_QUALITY_BLOCK,
        }
    }
}

/// True when `tag` (lowercased, trimmed) is a known family quality tag from either dialect.
fn is_quality_tag(tag: &str) -> bool {
    PONY_QUALITY_TAGS.contains(&tag) || ILLUSTRIOUS_QUALITY_TAGS.contains(&tag)
}

/// Deterministic family swap: drop any known quality tags, prepend `target`'s quality block.
/// Pure and host-testable — the LLM path handles the fuzzier content rewriting.
pub fn convert_family(prompt: &str, target: PromptFamily) -> String {
    let mut kept: Vec<String> = Vec::new();
    for raw in prompt.split(',') {
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        if is_quality_tag(&tag.to_ascii_lowercase()) {
            continue;
        }
        kept.push(tag.to_string());
    }
    let block = target.quality_block();
    if kept.is_empty() {
        return block.to_string();
    }
    format!("{block}, {}", kept.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_prompt_wraps_system_and_user_turns() {
        let p = build_prompt("SYS", "USER");
        assert_eq!(
            p,
            "<|im_start|>system\nSYS<|im_end|>\n<|im_start|>user\nUSER<|im_end|>\n<|im_start|>assistant\n"
        );
        // The assistant turn is left open for generation (no trailing im_end).
        assert!(p.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn kinds_map_to_their_system_prompts() {
        assert_eq!(RewriteKind::TagsToVideo.system(), SYS_TAGS_TO_VIDEO);
        assert_eq!(RewriteKind::ProseToTags.system(), SYS_PROSE_TO_TAGS);
        assert_eq!(RewriteKind::ToPony.system(), SYS_FAMILY_TO_PONY);
        assert_eq!(RewriteKind::ToIllustrious.system(), SYS_FAMILY_TO_ILLUSTRIOUS);
        assert_eq!(RewriteKind::ToAnima.system(), SYS_FAMILY_TO_ANIMA);
        // Each system prompt is real guidance, not a placeholder.
        for k in [
            RewriteKind::TagsToVideo,
            RewriteKind::ProseToTags,
            RewriteKind::ToPony,
            RewriteKind::ToAnima,
        ] {
            assert!(k.system().len() > 40);
        }
    }

    #[test]
    fn anima_prompt_avoids_quality_blocks_and_asks_for_hybrid() {
        let sys = RewriteKind::ToAnima.system();
        assert_eq!(RewriteKind::ToAnima.label(), "To Anima");
        // No score_/masterpiece quality block; leads with natural language then tags.
        assert!(!sys.contains("score_9"));
        assert!(sys.contains("do not add any quality block"));
        assert!(sys.contains("natural-language"));
        assert!(sys.contains("comma-separated tags"));
    }

    #[test]
    fn assembling_a_kind_embeds_its_system_prompt() {
        let p = build_prompt(RewriteKind::TagsToVideo.system(), "1girl, running, rain");
        assert!(p.contains(SYS_TAGS_TO_VIDEO));
        assert!(p.contains("1girl, running, rain"));
        assert!(p.starts_with("<|im_start|>system\n"));
    }

    #[test]
    fn family_swap_replaces_pony_block_with_illustrious() {
        let src = "score_9, score_8_up, 1girl, blue_hair, smile";
        let out = convert_family(src, PromptFamily::Illustrious);
        assert!(out.starts_with(ILLUSTRIOUS_QUALITY_BLOCK));
        assert!(out.contains("1girl"));
        assert!(out.contains("blue_hair"));
        assert!(!out.contains("score_9"));
    }

    #[test]
    fn family_swap_replaces_illustrious_block_with_pony() {
        let src = "masterpiece, best quality, 1boy, armor";
        let out = convert_family(src, PromptFamily::Pony);
        assert!(out.starts_with(PONY_QUALITY_BLOCK));
        assert!(out.contains("1boy"));
        assert!(out.contains("armor"));
        assert!(!out.to_ascii_lowercase().contains("masterpiece"));
    }

    #[test]
    fn family_swap_on_bare_content_just_prepends_block() {
        let out = convert_family("1girl, solo", PromptFamily::Pony);
        assert_eq!(out, format!("{PONY_QUALITY_BLOCK}, 1girl, solo"));
        // Empty input yields just the block.
        assert_eq!(convert_family("   ", PromptFamily::Pony), PONY_QUALITY_BLOCK);
    }

    /// The reported degeneration: the quality block followed by one phrase echoed ~20 times.
    /// Dedupe must collapse the echoes and keep first occurrences in order.
    #[test]
    fn dedupe_collapses_a_degenerate_decode() {
        let bad = format!(
            "{ILLUSTRIOUS_QUALITY_BLOCK}, illustrious NoobAI{}",
            ", illustrious NoobAI".repeat(20)
        );
        let out = dedupe_comma_segments(&bad);
        assert_eq!(out, format!("{ILLUSTRIOUS_QUALITY_BLOCK}, illustrious NoobAI"));
        // Case-insensitive, order-preserving, trims blanks.
        assert_eq!(dedupe_comma_segments("1girl, Solo, solo, , 1girl"), "1girl, Solo");
        assert_eq!(dedupe_comma_segments(""), "");
    }

    /// Only free-prose video output skips tag dedupe.
    #[test]
    fn tag_kinds_opt_into_dedupe() {
        assert!(!RewriteKind::TagsToVideo.dedupes_tags());
        for k in [
            RewriteKind::ProseToTags,
            RewriteKind::ToPony,
            RewriteKind::ToIllustrious,
            RewriteKind::ToAnima,
        ] {
            assert!(k.dedupes_tags());
        }
    }

    /// The family prompts each carry a worked example ending in tag output — the anchor that
    /// keeps the 0.5B model emitting tags instead of parroting the instructions.
    #[test]
    fn family_prompts_carry_examples() {
        for sys in [SYS_FAMILY_TO_PONY, SYS_FAMILY_TO_ILLUSTRIOUS, SYS_FAMILY_TO_ANIMA] {
            assert!(sys.contains("Example input:"));
            assert!(sys.contains("Example output:"));
        }
        assert!(SYS_FAMILY_TO_PONY.contains("Example output: score_9"));
        assert!(SYS_FAMILY_TO_ILLUSTRIOUS.contains("Example output: masterpiece"));
        // Anima's example must not smuggle a quality block back in.
        let example = SYS_FAMILY_TO_ANIMA.split("Example output:").nth(1).unwrap();
        assert!(!example.contains("masterpiece"));
        assert!(!example.contains("score_"));
    }
}
