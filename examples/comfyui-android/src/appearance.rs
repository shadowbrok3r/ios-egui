//! Prompt-tag classification for the look axes: which chips of a prompt describe the character's
//! appearance (hair, eyes, nails, skin, body — with swap families), and which belong to the
//! outfit / pose / camera / environment axes. Appearance runs on curated exact families plus a
//! compound-hair rule; the other axes add last-word and prefix patterns so color/length prefixes
//! ("black choker", "long white dress") still classify. Unknown tags never classify, so typos
//! never extract. Pure: no egui/android deps.

use crate::tags::{self, fold};
use crate::types::LookKind;
use std::collections::HashSet;

/// Grouping bucket for one appearance tag, in extract-sheet display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrSlot {
    HairColor,
    HairStyle,
    Eyes,
    Nails,
    Toenails,
    Skin,
    Body,
}

/// One curated tag family. Every member classifies into `slot`; `swap` families additionally feed
/// the chip swap strip, so their members must read as alternatives to each other.
struct Family {
    slot: AttrSlot,
    swap: bool,
    tags: &'static [&'static str],
}

const FAMILIES: &[Family] = &[
    Family {
        slot: AttrSlot::HairColor,
        swap: true,
        tags: &[
            "black hair", "blonde hair", "platinum blonde hair", "brown hair",
            "light brown hair", "red hair", "orange hair", "pink hair", "purple hair",
            "light purple hair", "blue hair", "dark blue hair", "light blue hair", "aqua hair",
            "green hair", "dark green hair", "light green hair", "grey hair", "silver hair",
            "white hair", "two-tone hair", "multicolored hair", "gradient hair", "streaked hair",
            "colored inner hair",
        ],
    },
    Family {
        slot: AttrSlot::HairStyle,
        swap: true,
        tags: &[
            "very short hair", "short hair", "medium hair", "long hair", "very long hair",
            "absurdly long hair",
        ],
    },
    Family {
        slot: AttrSlot::HairStyle,
        swap: true,
        tags: &[
            "straight hair", "wavy hair", "curly hair", "messy hair", "spiked hair",
            "flipped hair", "hair down", "bob cut", "inverted bob", "hime cut", "pixie cut",
            "ponytail", "high ponytail", "low ponytail", "side ponytail", "folded ponytail",
            "braided ponytail", "twintails", "short twintails", "low twintails", "braid",
            "single braid", "twin braids", "side braid", "french braid", "crown braid",
            "drill hair", "twin drills", "hair bun", "single hair bun", "double bun",
            "half updo", "topknot",
        ],
    },
    Family {
        slot: AttrSlot::HairStyle,
        swap: true,
        tags: &[
            "bangs", "blunt bangs", "parted bangs", "swept bangs", "crossed bangs",
            "asymmetrical bangs", "long bangs", "hair over one eye", "hair between eyes",
        ],
    },
    Family {
        slot: AttrSlot::HairStyle,
        swap: false,
        tags: &["ahoge", "antenna hair", "hair intakes", "sidelocks", "hair flaps"],
    },
    Family {
        slot: AttrSlot::Eyes,
        swap: true,
        tags: &[
            "amber eyes", "aqua eyes", "black eyes", "blue eyes", "brown eyes", "green eyes",
            "grey eyes", "orange eyes", "pink eyes", "purple eyes", "violet eyes", "red eyes",
            "silver eyes", "white eyes", "yellow eyes", "heterochromia", "multicolored eyes",
        ],
    },
    Family {
        slot: AttrSlot::Eyes,
        swap: false,
        tags: &["slit pupils", "glowing eyes", "tsurime", "tareme"],
    },
    Family {
        slot: AttrSlot::Nails,
        swap: true,
        tags: &[
            "nail polish", "aqua nails", "black nails", "blue nails", "brown nails",
            "green nails", "grey nails", "orange nails", "pink nails", "purple nails",
            "red nails", "white nails", "yellow nails", "multicolored nails",
        ],
    },
    Family {
        slot: AttrSlot::Nails,
        swap: false,
        tags: &["fingernails", "long fingernails", "nail art"],
    },
    Family {
        slot: AttrSlot::Toenails,
        swap: true,
        tags: &[
            "toenail polish", "aqua toenails", "black toenails", "blue toenails",
            "brown toenails", "green toenails", "grey toenails", "orange toenails",
            "pink toenails", "purple toenails", "red toenails", "white toenails",
            "yellow toenails", "multicolored toenails",
        ],
    },
    Family {
        slot: AttrSlot::Toenails,
        swap: false,
        tags: &["toenails", "long toenails"],
    },
    Family {
        slot: AttrSlot::Skin,
        swap: true,
        tags: &["pale skin", "fair skin", "tan", "dark skin", "very dark skin", "dark-skinned female"],
    },
    Family {
        slot: AttrSlot::Skin,
        swap: false,
        tags: &[
            "tanlines", "freckles", "body freckles", "shiny skin", "soft skin", "wet skin",
            "oily skin", "glossy skin", "mole", "mole under eye",
            "sweat", "sweaty", "mole under mouth", "mole on cheek", "mole on neck",
            "mole on breast", "tattoo",
            "arm tattoo", "leg tattoo", "back tattoo", "chest tattoo", "shoulder tattoo",
            "neck tattoo", "stomach tattoo", "facial tattoo", "pubic tattoo", "scar",
            "scar across eye", "scar on face", "scar on cheek",
        ],
    },
    Family {
        slot: AttrSlot::Body,
        swap: true,
        tags: &[
            "flat chest", "small breasts", "medium breasts", "large breasts", "huge breasts",
            "gigantic breasts",
        ],
    },
    Family {
        slot: AttrSlot::Body,
        swap: true,
        tags: &[
            "petite", "skinny", "curvy", "curvy body", "plump", "chubby", "toned", "muscular",
            "muscular female", "abs", "tall", "tall female", "mature female", "mature woman",
            "milf", "slender build",
        ],
    },
    Family {
        slot: AttrSlot::Body,
        swap: true,
        tags: &["big butt", "bubble butt", "huge bubble butt", "huge ass"],
    },
    Family {
        slot: AttrSlot::Body,
        swap: false,
        tags: &["thick thighs", "wide hips", "narrow waist", "slim waist", "slim thighs"],
    },
    Family {
        slot: AttrSlot::Body,
        swap: false,
        tags: &[
            "erect nipples", "large areolas", "large areolae", "natural breasts", "natural tits",
            "huge bust", "large saggy breasts", "sagging breasts", "adult woman",
            "long eyelashes", "thick eyelashes", "feet", "foot", "toes", "soles", "foot sole",
            "cute feet", "armpit hair", "pubic hair",
        ],
    },
];

/// The family containing `folded`, if any.
fn family_of(folded: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| f.tags.contains(&folded))
}

/// Word sets for compound hair tags no exact family carries ("long straight hair").
const HAIR_COLOR_WORDS: &[&str] = &[
    "aqua", "black", "blonde", "platinum", "blue", "brown", "red", "orange", "pink", "purple",
    "green", "grey", "gray", "silver", "white", "rainbow", "dark", "light", "two-tone",
    "multicolored", "gradient", "streaked",
];
const HAIR_STYLE_WORDS: &[&str] = &[
    "very", "long", "short", "medium", "absurdly", "straight", "wavy", "curly", "messy",
    "spiked", "spiky", "flipped", "wet", "big", "stylish",
];

/// Compound-hair rule: `… hair` where every word is a known color/style word. Any color word
/// makes it HairColor, else HairStyle.
fn classify_hair_compound(folded: &str) -> Option<AttrSlot> {
    let rest = folded.strip_suffix(" hair")?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.is_empty()
        || !words
            .iter()
            .all(|w| HAIR_COLOR_WORDS.contains(w) || HAIR_STYLE_WORDS.contains(w))
    {
        return None;
    }
    if words.iter().any(|w| HAIR_COLOR_WORDS.contains(w)) {
        Some(AttrSlot::HairColor)
    } else {
        Some(AttrSlot::HairStyle)
    }
}

/// Non-human character features, matched on the tag's last word ("cat ears", "demon horns").
const BODY_LAST_WORDS: &[&str] = &["ears", "tail", "wings", "horn", "horns", "halo", "fang", "fangs"];

/// Whether `folded`'s last word is in `words`.
fn last_word_in(folded: &str, words: &[&str]) -> bool {
    folded.split_whitespace().next_back().is_some_and(|w| words.contains(&w))
}

/// The appearance slot `tag` belongs to, if it is a known appearance tag.
pub fn classify(tag: &str) -> Option<AttrSlot> {
    let f = fold(tag);
    if let Some(fam) = family_of(&f) {
        return Some(fam.slot);
    }
    if let Some(slot) = classify_hair_compound(&f) {
        return Some(slot);
    }
    // Any "<x> bangs", and "hair <position>" phrasings ("hair past shoulders") — but not hair
    // accessories, whose last word is a garment noun ("hair ornament").
    if last_word_in(&f, &["bangs"]) {
        return Some(AttrSlot::HairStyle);
    }
    if f.starts_with("hair ") && !last_word_in(&f, OUTFIT_LAST_WORDS) {
        return Some(AttrSlot::HairStyle);
    }
    last_word_in(&f, BODY_LAST_WORDS).then_some(AttrSlot::Body)
}

/// Swap alternatives for `tag`: its swap family's members (the tag itself included), or the
/// expression swap set for a swappable expression.
pub fn variants(tag: &str) -> Option<&'static [&'static str]> {
    let f = fold(tag);
    if let Some(fam) = family_of(&f).filter(|f| f.swap) {
        return Some(fam.tags);
    }
    EXPRESSION_SWAP.contains(&f.as_str()).then_some(EXPRESSION_SWAP)
}

/// Facial expressions that read as alternatives — one swap family, also fed to the chip swap
/// strip via [`variants`].
const EXPRESSION_SWAP: &[&str] = &[
    "smile", "grin", "smirk", "smug", "frown", "pout", "angry", "annoyed", "sad", "crying",
    "happy", "laughing", "surprised", "embarrassed", "shy", "nervous", "sleepy", "bored",
    "serious", "expressionless", "blush", "cringe", "ahegao", "seductive expression",
    "neutral expression",
];

/// Expression tags that accompany rather than replace one another (mouth/eye states, moods).
const EXPRESSION_EXACT: &[&str] = &[
    "open mouth", "closed mouth", "parted lips", "tongue out", "tears", "teary eyes",
    "one eye closed", "wink", "half-closed eyes", "eyes closed", "looking away", "disgust",
    "scared", "worried", "confused", "drooling", "clenched teeth", "seductive look",
    "sexual expression",
];

/// Garment / accessory nouns, matched on the last word so color and cut prefixes ride along
/// ("black choker", "pleated skirt", "toe ring").
const OUTFIT_LAST_WORDS: &[&str] = &[
    "uniform", "serafuku", "dress", "sundress", "gown", "skirt", "shirt", "t-shirt", "blouse",
    "top", "camisole", "sweater", "hoodie", "cardigan", "vest", "coat", "jacket", "blazer",
    "suit", "tuxedo", "kimono", "yukata", "hanbok", "hakama", "obi", "sash", "apron", "leotard",
    "bodysuit", "swimsuit", "bikini", "lingerie", "bra", "panties", "underwear", "pajamas",
    "sleepwear", "nightgown", "robe", "cape", "cloak", "armor", "breastplate", "pauldrons",
    "gauntlets", "corset", "overalls", "jeans", "denim", "pants", "trousers", "shorts",
    "buruma", "bloomers", "pantyhose", "tights", "thighhighs", "kneehighs", "socks", "legwear",
    "garter", "fishnets", "boots", "heels", "sandals", "sneakers", "loafers", "shoes",
    "footwear", "gloves", "mittens", "scarf", "necktie", "tie", "bowtie", "ribbon", "bow",
    "choker", "necklace", "pendant", "earrings", "bracelet", "ring", "jewelry", "crown",
    "tiara", "hat", "cap", "beret", "bonnet", "hood", "headdress", "headband", "hairband",
    "hairclip", "hairpin", "ornament", "scrunchie", "earmuffs", "glasses", "sunglasses",
    "eyewear", "goggles", "mask", "veil", "collar", "belt", "zipper", "clothes", "costume",
    "fashion", "wear", "makeup", "print", "sleeves", "frills", "lace", "epaulettes",
];

/// Outfit tags the last-word rule can't reach.
const OUTFIT_EXACT: &[&str] = &[
    "office lady", "casual", "maid", "nurse", "miko", "gothic lolita", "goth fashion", "gothic",
    "punk", "sportswear", "tracksuit", "techwear", "idol", "playboy bunny", "cheerleader",
    "western", "knight", "witch", "police", "side slit", "skin tight", "bare shoulders",
    "detached collar", "pom poms", "crop top", "high heels", "neon trim", "gears",
    "studded belt", "naked", "nude", "topless", "bottomless", "bare body", "skimpy", "barefoot",
];

/// Pose tags: postures, gestures, and their common danbooru phrasings.
const POSE_EXACT: &[&str] = &[
    "standing", "contrapposto", "sitting", "sitting on floor", "seiza", "kneeling", "squatting",
    "crouching", "lying", "on back", "on stomach", "on side", "walking", "running", "jumping",
    "mid-air", "dancing", "twirling", "dress flip", "stretching", "leaning forward",
    "leaning back", "relaxed", "looking back", "looking over shoulder",
    "head tilt", "head rest", "waving", "one arm up", "peace sign", "v", "pointing at viewer",
    "reaching towards viewer", "outstretched arm", "blowing kiss", "cheering", "skirt hold",
    "curtsy", "salute", "dynamic pose", "feet up", "crossed legs", "crossed arms",
    "lying on back", "lying on side", "lying on stomach", "laying on back", "laying on side",
    "laying in bed", "lying in bed", "leaning on elbow", "deep squat", "yoga pose",
    "squatting cowgirl position", "girl on top", "one leg up", "spread toes", "splayed toes",
    "curled toes", "curling toes", "pointed toes", "spreading her toes",
    "holding knees to chest", "holding legs to chest", "holding one leg",
];

/// Pose prefixes: any tag starting with one of these reads as a body/limb position.
const POSE_PREFIXES: &[&str] =
    &["arms ", "hands ", "hand ", "legs ", "leg ", "hugging ", "toes ", "knees "];

/// Camera / framing tags.
const CAMERA_EXACT: &[&str] = &[
    "eye level", "straight-on", "from below", "from above", "from side", "from behind",
    "from front", "low angle", "high angle", "dutch angle", "dynamic angle", "tilted",
    "bird's-eye view", "worm's-eye view", "back view", "front view", "side view",
    "three-quarter view", "facing viewer", "looking at viewer", "close-up", "extreme close-up",
    "portrait",
    "upper body", "lower body", "full body", "cowboy shot", "wide shot", "pov", "selfie",
    "profile", "over the shoulder", "fisheye", "wide-angle lens", "depth of field", "bokeh",
    "blurry background", "blurry foreground", "foreshortening", "arm at own side extended",
    "point of view", "full body view", "full body image", "profile view", "ass view",
    "front facing", "up close", "off center",
];

/// Camera nouns, matched on the last word ("face focus", "crotch shot").
const CAMERA_LAST_WORDS: &[&str] = &["focus", "shot", "angle", "lens"];

/// Scene tags the last-word rule can't reach.
const ENV_EXACT: &[&str] = &[
    "outdoors", "indoors", "scenery", "landscape", "nature", "cyberpunk", "sci-fi", "fantasy",
    "steampunk", "spring", "summer", "winter", "autumn", "rain", "raining", "snow", "snowing",
    "fog", "mist", "wind", "underwater", "serene", "festive", "cozy", "epic scale",
    "milky way", "city passing by", "medieval", "crowd", "overcast", "overgrown",
    "stained glass", "moonlit", "steamy", "visible breath",
];

/// Scene nouns, matched on the last word ("neon lights", "cherry blossoms", "city view").
const ENV_LAST_WORDS: &[&str] = &[
    "street", "city", "cityscape", "skyline", "town", "alley", "rooftop", "park", "forest",
    "woods", "tree", "trees", "bamboo", "garden", "meadow", "field", "grassland", "hill",
    "mountain", "mountains", "beach", "ocean", "sea", "lake", "river", "waterfall", "pool",
    "poolside", "onsen", "sky", "cloud", "clouds", "stars", "nebula", "galaxy", "space",
    "night", "day", "morning", "evening", "dusk", "dawn", "sunset", "sunrise", "sunlight",
    "moonlight", "moon", "sun", "hour", "light", "lights", "lighting", "rays", "reflections",
    "petals", "blossoms", "leaves", "maple", "moss", "stone", "rocks", "steam", "sand",
    "dunes", "desert", "cave", "bridge", "harbor", "pier", "classroom", "school", "library",
    "cafe", "restaurant", "shop", "market", "stalls", "shrine", "temple", "torii", "castle",
    "ruins", "cathedral", "church", "architecture", "bedroom", "room", "kitchen", "bathroom",
    "bathtub", "office", "train", "station", "bus", "spaceship", "interior", "panels",
    "window", "curtains", "tatami", "shoji", "futon", "bed", "desks", "bookshelves",
    "banners", "holograms", "skyscrapers", "seats", "lanterns", "fireworks", "festival",
    "wheel", "islands", "atmosphere", "tones", "view", "background", "walls", "path",
    "umbrella", "way", "spring", "signs", "plants", "bubbles", "landscape", "sheets", "shower",
];

/// The look axis `tag` belongs to, if recognized: appearance families and word rules first,
/// then each axis' exact set, then the noun / prefix patterns (broadest last).
pub fn classify_axis(tag: &str) -> Option<LookKind> {
    let f = fold(tag);
    // Prose fragments (video prompts, sentence-style chips) never classify: real danbooru tags
    // top out around five words, and pulling half a sentence out of a prompt breaks it.
    if f.split_whitespace().count() > 5 {
        return None;
    }
    if classify(&f).is_some() {
        return Some(LookKind::Appearance);
    }
    if EXPRESSION_SWAP.contains(&f.as_str()) || EXPRESSION_EXACT.contains(&f.as_str()) {
        return Some(LookKind::Expression);
    }
    if OUTFIT_EXACT.contains(&f.as_str()) {
        return Some(LookKind::Outfit);
    }
    if POSE_EXACT.contains(&f.as_str()) {
        return Some(LookKind::Pose);
    }
    if CAMERA_EXACT.contains(&f.as_str()) {
        return Some(LookKind::CameraAngle);
    }
    if ENV_EXACT.contains(&f.as_str()) {
        return Some(LookKind::Environment);
    }
    if last_word_in(&f, OUTFIT_LAST_WORDS) {
        return Some(LookKind::Outfit);
    }
    if last_word_in(&f, CAMERA_LAST_WORDS) {
        return Some(LookKind::CameraAngle);
    }
    if last_word_in(&f, &["expression", "face"]) {
        return Some(LookKind::Expression);
    }
    if last_word_in(&f, ENV_LAST_WORDS) {
        return Some(LookKind::Environment);
    }
    if POSE_PREFIXES.iter().any(|p| f.starts_with(p)) {
        return Some(LookKind::Pose);
    }
    None
}

/// One classified chip pulled out of a prompt: the verbatim span (weight wrapper intact), the
/// peeled tag, its axis, and — for Appearance — the sub-slot used to group the sheet.
pub struct Extracted {
    pub text: String,
    pub tag: String,
    pub kind: LookKind,
    pub slot: Option<AttrSlot>,
}

/// The classified chips of `text`, in prompt order, across every look axis.
pub fn extract(text: &str) -> Vec<Extracted> {
    tags::parse_chips(text)
        .into_iter()
        .filter_map(|c| {
            classify_axis(&c.tag).map(|kind| Extracted {
                text: text[c.range.clone()].to_string(),
                slot: (kind == LookKind::Appearance).then(|| classify(&c.tag)).flatten(),
                tag: c.tag,
                kind,
            })
        })
        .collect()
}

/// Remove every chip whose folded tag is in `remove`, keeping the rest verbatim.
pub fn remove_tags(text: &str, remove: &HashSet<String>) -> String {
    let victims: Vec<usize> = tags::parse_chips(text)
        .iter()
        .enumerate()
        .filter(|(_, c)| remove.contains(&fold(&c.tag)))
        .map(|(i, _)| i)
        .collect();
    let mut out = text.to_string();
    for &i in victims.iter().rev() {
        out = tags::remove_chip(&out, i);
    }
    out
}

/// Preset-name suggestion for one axis' extracted tags: hair color + eyes for Appearance when
/// present, else the axis' first two tags.
pub fn axis_name(kind: LookKind, items: &[Extracted]) -> String {
    let of_kind = || items.iter().filter(|e| e.kind == kind);
    if kind == LookKind::Appearance {
        let hair = of_kind().find(|e| e.slot == Some(AttrSlot::HairColor));
        let eyes = of_kind().find(|e| e.slot == Some(AttrSlot::Eyes));
        match (hair, eyes) {
            (Some(h), Some(e)) => return format!("{}, {}", h.tag, e.tag),
            (Some(h), None) => return h.tag.clone(),
            (None, Some(e)) => return e.tag.clone(),
            (None, None) => {}
        }
    }
    of_kind().take(2).map(|e| e.tag.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_hits_appearance_and_skips_everything_else() {
        assert_eq!(classify("silver hair"), Some(AttrSlot::HairColor));
        assert_eq!(classify("Silver_Hair"), Some(AttrSlot::HairColor));
        assert_eq!(classify("twintails"), Some(AttrSlot::HairStyle));
        assert_eq!(classify("long hair"), Some(AttrSlot::HairStyle));
        assert_eq!(classify("red eyes"), Some(AttrSlot::Eyes));
        assert_eq!(classify("black nails"), Some(AttrSlot::Nails));
        assert_eq!(classify("toenail polish"), Some(AttrSlot::Toenails));
        assert_eq!(classify("red toenails"), Some(AttrSlot::Toenails));
        assert_eq!(classify("dark skin"), Some(AttrSlot::Skin));
        assert_eq!(classify("large breasts"), Some(AttrSlot::Body));
        assert_eq!(classify("thick thighs"), Some(AttrSlot::Body));

        for not_appearance in [
            "1girl", "school uniform", "black dress", "long sleeves", "short shorts",
            "red scarf", "smile", "outdoors", "masterpiece", "looking at viewer", "silvr hair",
        ] {
            assert_eq!(classify(not_appearance), None, "{not_appearance}");
        }
    }

    #[test]
    fn variants_swap_within_family_only() {
        let hair = variants("blonde hair").unwrap();
        assert!(hair.contains(&"silver hair"));
        assert!(!hair.contains(&"long hair"));

        let ladder = variants("flat chest").unwrap();
        assert_eq!(ladder.last(), Some(&"gigantic breasts"));

        // Classify-only families never offer swaps.
        assert!(variants("long fingernails").is_none());
        assert!(variants("freckles").is_none());
        assert!(variants("school uniform").is_none());
    }

    #[test]
    fn extract_keeps_weight_wrappers_verbatim() {
        let text = "1girl, (silver hair:1.2), school uniform, red eyes, black toenails";
        let got = extract(text);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].text, "(silver hair:1.2)");
        assert_eq!(got[0].tag, "silver hair");
        assert_eq!(got[0].kind, LookKind::Appearance);
        assert_eq!(got[1].tag, "school uniform");
        assert_eq!(got[1].kind, LookKind::Outfit);
        assert_eq!(got[1].slot, None);
        assert_eq!(got[2].tag, "red eyes");
        assert_eq!(got[3].slot, Some(AttrSlot::Toenails));
    }

    #[test]
    fn the_2026_08_13_review_misses_now_classify() {
        for (tag, kind) in [
            ("ahoge", LookKind::Appearance),
            ("aqua hair", LookKind::Appearance),
            ("shiny skin", LookKind::Appearance),
            ("huge breasts", LookKind::Appearance),
            ("aqua toenails", LookKind::Appearance),
            ("long straight hair", LookKind::Appearance),
            ("soft skin", LookKind::Appearance),
            ("wet skin", LookKind::Appearance),
            ("hair ornament", LookKind::Outfit),
            ("black choker", LookKind::Outfit),
            ("toe ring", LookKind::Outfit),
        ] {
            assert_eq!(classify_axis(tag), Some(kind), "{tag}");
        }
        assert_eq!(classify("long straight hair"), Some(AttrSlot::HairStyle));
        assert_eq!(classify("dark blue hair"), Some(AttrSlot::HairColor));
        assert_eq!(classify("aqua toenails"), Some(AttrSlot::Toenails));
    }

    /// High-frequency tags from the 2026-08-13 on-server corpus run (2241 images, 336 distinct
    /// prompts): the classifier must place these, and must NOT touch prose, quality tags,
    /// character names, or LoRA triggers.
    #[test]
    fn corpus_round_one_classifies() {
        for (tag, kind) in [
            ("violet eyes", LookKind::Appearance),
            ("fair skin", LookKind::Appearance),
            ("erect nipples", LookKind::Appearance),
            ("curvy body", LookKind::Appearance),
            ("bubble butt", LookKind::Appearance),
            ("large saggy breasts", LookKind::Appearance),
            ("slim waist", LookKind::Appearance),
            ("cute bangs", LookKind::Appearance),
            ("hair past shoulders", LookKind::Appearance),
            ("long eyelashes", LookKind::Appearance),
            ("sweaty", LookKind::Appearance),
            ("naked", LookKind::Outfit),
            ("gothic", LookKind::Outfit),
            ("no shoes", LookKind::Outfit),
            ("girl on top", LookKind::Pose),
            ("knees tucked", LookKind::Pose),
            ("toes splayed", LookKind::Pose),
            ("pointed toes", LookKind::Pose),
            ("laying on back", LookKind::Pose),
            ("holding knees to chest", LookKind::Pose),
            ("point of view", LookKind::CameraAngle),
            ("full body view", LookKind::CameraAngle),
            ("front facing", LookKind::CameraAngle),
            ("feet focus", LookKind::CameraAngle),
            ("moonlit", LookKind::Environment),
            ("dark bedroom", LookKind::Environment),
            ("soft studio lighting", LookKind::Environment),
            ("ahegao", LookKind::Expression),
            ("annoyed", LookKind::Expression),
            ("cringe", LookKind::Expression),
            ("blush", LookKind::Expression),
            ("smirk", LookKind::Expression),
            ("seductive expression", LookKind::Expression),
        ] {
            assert_eq!(classify_axis(tag), Some(kind), "{tag}");
        }
        for none in [
            "hatsune miku", "fjcump", "smoothmixanime", "score 9",
            "tonails", "skindention",
            "wearing a plain grey crew-neck t-shirt and blue jeans",
            "her tits bounce out of her shirt",
            "panties around her waist and hands through the sides of the panties",
        ] {
            assert_eq!(classify_axis(none), None, "{none}");
        }
    }

    #[test]
    fn axis_classification_samples() {
        for (tag, kind) in [
            ("school uniform", LookKind::Outfit),
            ("pleated skirt", LookKind::Outfit),
            ("hand on hip", LookKind::Pose),
            ("sitting", LookKind::Pose),
            ("from below", LookKind::CameraAngle),
            ("cowboy shot", LookKind::CameraAngle),
            ("neon lights", LookKind::Environment),
            ("cherry blossoms", LookKind::Environment),
            ("night", LookKind::Environment),
            ("cat ears", LookKind::Appearance),
            ("fox tail", LookKind::Appearance),
            ("smile", LookKind::Expression),
            ("half-closed eyes", LookKind::Expression),
            ("pained expression", LookKind::Expression),
        ] {
            assert_eq!(classify_axis(tag), Some(kind), "{tag}");
        }
        for none in ["1girl", "solo", "masterpiece", "best quality", "green", "holding sword"] {
            assert_eq!(classify_axis(none), None, "{none}");
        }
    }

    /// Every tag in every builtin look preset must classify to that preset's axis — the presets
    /// are the vocabulary's ground truth. Cross-axis or judgment tags carry their accepted value.
    #[test]
    fn builtin_look_prompts_classify_to_their_axis() {
        let exceptions: &[(&str, Option<LookKind>)] = &[
            ("rabbit ears", Some(LookKind::Appearance)),
            ("smile", Some(LookKind::Expression)),
            ("green", None),
            ("summer festival", Some(LookKind::Environment)),
            ("fantasy", Some(LookKind::Environment)),
            ("cyberpunk", Some(LookKind::Environment)),
            ("steampunk", Some(LookKind::Environment)),
            ("scenery", Some(LookKind::Environment)),
            ("from behind", Some(LookKind::CameraAngle)),
            ("casual", Some(LookKind::Outfit)),
        ];
        let mut failures = Vec::new();
        for look in crate::types::builtin_looks() {
            for chip in tags::parse_chips(&look.prompt) {
                let got = classify_axis(&chip.tag);
                if got == Some(look.kind) {
                    continue;
                }
                let folded = fold(&chip.tag);
                if exceptions.iter().any(|(t, ok)| *t == folded && got == *ok) {
                    continue;
                }
                failures.push(format!(
                    "{:?} '{}' in [{} {}] -> {:?}",
                    look.kind, chip.tag, look.name, look.kind.label(), got
                ));
            }
        }
        assert!(failures.is_empty(), "{} misclassified:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn remove_tags_drops_selected_and_keeps_rest() {
        let text = "1girl, (silver hair:1.2), school uniform, red eyes, smile";
        let remove: HashSet<String> =
            ["silver hair", "red eyes"].iter().map(|s| s.to_string()).collect();
        assert_eq!(remove_tags(text, &remove), "1girl, school uniform, smile");
        assert_eq!(remove_tags(text, &HashSet::new()), text);
    }

    #[test]
    fn axis_name_prefers_hair_and_eyes() {
        let items = extract("large breasts, silver hair, red eyes, black nails, school uniform");
        assert_eq!(axis_name(LookKind::Appearance, &items), "silver hair, red eyes");
        assert_eq!(axis_name(LookKind::Outfit, &items), "school uniform");
        let items = extract("black nails, large breasts");
        assert_eq!(axis_name(LookKind::Appearance, &items), "black nails, large breasts");
    }

    /// Manual tuning harness over a real prompt corpus (one positive prompt per line):
    /// `APPEARANCE_CORPUS=<file> cargo test -p comfyui_android corpus_report -- --ignored --nocapture`
    /// Prints tag frequencies per axis plus the unclassified remainder, count-descending.
    #[test]
    #[ignore = "manual tuning harness; set APPEARANCE_CORPUS"]
    fn corpus_report() {
        use std::collections::HashMap;
        let path = std::env::var("APPEARANCE_CORPUS").expect("set APPEARANCE_CORPUS");
        let text = std::fs::read_to_string(&path).expect("read corpus");
        let mut counts: HashMap<Option<LookKind>, HashMap<String, u32>> = HashMap::new();
        for line in text.lines() {
            for chip in tags::parse_chips(line) {
                let f = fold(&chip.tag);
                if f.is_empty() {
                    continue;
                }
                *counts.entry(classify_axis(&f)).or_default().entry(f).or_insert(0) += 1;
            }
        }
        let dump = |title: &str, m: Option<&HashMap<String, u32>>, cap: usize| {
            let mut v: Vec<(&String, &u32)> = m.map(|m| m.iter().collect()).unwrap_or_default();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            println!("\n== {title} ({} distinct) ==", v.len());
            for (tag, n) in v.into_iter().take(cap) {
                println!("{n:>5}  {tag}");
            }
        };
        for kind in [
            LookKind::Appearance,
            LookKind::Outfit,
            LookKind::Pose,
            LookKind::CameraAngle,
            LookKind::Environment,
        ] {
            dump(kind.label(), counts.get(&Some(kind)), 60);
        }
        dump("UNCLASSIFIED", counts.get(&None), 200);
    }

    /// Family tables stay folded and unambiguous: no duplicates across families, and every entry
    /// already in fold() form so membership tests never miss.
    #[test]
    fn families_are_folded_and_disjoint() {
        let mut seen = HashSet::new();
        for f in FAMILIES {
            for t in f.tags {
                assert_eq!(*t, fold(t), "unfolded family entry: {t}");
                assert!(seen.insert(*t), "tag in two families: {t}");
            }
        }
    }
}
