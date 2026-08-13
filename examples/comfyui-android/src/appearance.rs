//! Appearance-attribute classification over prompt tags: which chips of a prompt describe the
//! character's look (hair, eyes, nails, skin, body) and what each can swap to. Membership in the
//! curated families below is the whole test — unknown tags never classify, so typos never
//! extract. Pure: no egui/android deps.

use crate::tags::{self, fold};
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

impl AttrSlot {
    pub const ALL: &'static [Self] = &[
        Self::HairColor,
        Self::HairStyle,
        Self::Eyes,
        Self::Nails,
        Self::Toenails,
        Self::Skin,
        Self::Body,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::HairColor => "Hair color",
            Self::HairStyle => "Hair style",
            Self::Eyes => "Eyes",
            Self::Nails => "Nails",
            Self::Toenails => "Toenails",
            Self::Skin => "Skin",
            Self::Body => "Body",
        }
    }
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
            "grey eyes", "orange eyes", "pink eyes", "purple eyes", "red eyes", "silver eyes",
            "white eyes", "yellow eyes", "heterochromia", "multicolored eyes",
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
            "toenail polish", "black toenails", "blue toenails", "green toenails",
            "orange toenails", "pink toenails", "purple toenails", "red toenails",
            "white toenails", "yellow toenails",
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
        tags: &["pale skin", "tan", "dark skin", "very dark skin", "dark-skinned female"],
    },
    Family {
        slot: AttrSlot::Skin,
        swap: false,
        tags: &[
            "tanlines", "freckles", "body freckles", "shiny skin", "mole", "mole under eye",
            "mole under mouth", "mole on cheek", "mole on neck", "mole on breast", "tattoo",
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
            "petite", "skinny", "curvy", "plump", "chubby", "toned", "muscular",
            "muscular female", "abs", "tall", "tall female", "mature female",
        ],
    },
    Family {
        slot: AttrSlot::Body,
        swap: false,
        tags: &["thick thighs", "wide hips", "narrow waist"],
    },
];

/// The family containing `folded`, if any.
fn family_of(folded: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| f.tags.contains(&folded))
}

/// The appearance slot `tag` belongs to, if it is a known appearance tag.
pub fn classify(tag: &str) -> Option<AttrSlot> {
    family_of(&fold(tag)).map(|f| f.slot)
}

/// Swap alternatives for `tag`: its swap family's members (the tag itself included).
pub fn variants(tag: &str) -> Option<&'static [&'static str]> {
    family_of(&fold(tag)).filter(|f| f.swap).map(|f| f.tags)
}

/// One appearance chip pulled out of a prompt: the verbatim span (weight wrapper intact), the
/// peeled tag, and its slot.
pub struct Extracted {
    pub text: String,
    pub tag: String,
    pub slot: AttrSlot,
}

/// The appearance chips of `text`, in prompt order.
pub fn extract(text: &str) -> Vec<Extracted> {
    tags::parse_chips(text)
        .into_iter()
        .filter_map(|c| {
            classify(&c.tag).map(|slot| Extracted {
                text: text[c.range.clone()].to_string(),
                tag: c.tag,
                slot,
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

/// Extract-sheet name suggestion: hair color + eyes when present, else the first two tags.
pub fn suggest_name(items: &[Extracted]) -> String {
    let hair = items.iter().find(|e| e.slot == AttrSlot::HairColor);
    let eyes = items.iter().find(|e| e.slot == AttrSlot::Eyes);
    let picks: Vec<&str> = match (&hair, &eyes) {
        (Some(h), Some(e)) => vec![h.tag.as_str(), e.tag.as_str()],
        (Some(h), None) => vec![h.tag.as_str()],
        (None, Some(e)) => vec![e.tag.as_str()],
        (None, None) => items.iter().take(2).map(|e| e.tag.as_str()).collect(),
    };
    picks.join(", ")
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
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].text, "(silver hair:1.2)");
        assert_eq!(got[0].tag, "silver hair");
        assert_eq!(got[1].tag, "red eyes");
        assert_eq!(got[2].slot, AttrSlot::Toenails);
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
    fn suggest_name_prefers_hair_and_eyes() {
        let items = extract("large breasts, silver hair, red eyes, black nails");
        assert_eq!(suggest_name(&items), "silver hair, red eyes");
        let items = extract("black nails, large breasts");
        assert_eq!(suggest_name(&items), "black nails, large breasts");
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
