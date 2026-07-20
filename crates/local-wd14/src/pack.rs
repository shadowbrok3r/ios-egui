//! WD14 tagger pack on disk: the HTP context binary and the selected-tags CSV,
//! keyed by the `WD14` marker file.

use crate::error::{Error, Result};
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// The marker file that identifies a WD14 tagger pack.
pub const MARKER: &str = "WD14";

/// Files every pack must have.
pub const REQUIRED: [&str; 3] = [MARKER, "model.bin", "tags.csv"];

/// Danbooru category: general descriptive tag.
pub const CATEGORY_GENERAL: u8 = 0;
/// Danbooru category: character/identity tag.
pub const CATEGORY_CHARACTER: u8 = 4;
/// Danbooru category: rating tag (general/sensitive/questionable/explicit).
pub const CATEGORY_RATING: u8 = 9;

/// One CSV row: the output index it sits at maps to the classifier's logit index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wd14Tag {
    pub name: String,
    pub category: u8,
    pub count: u32,
}

impl Wd14Tag {
    /// Prompt insertion form: underscores rendered as spaces.
    pub fn insert_text(&self) -> String {
        self.name.replace('_', " ")
    }
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

/// Parse `tag_id,name,category,count` rows in file order, skipping a leading header row.
pub fn parse_tags_csv(text: &str) -> Result<Vec<Wd14Tag>> {
    let mut tags = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let fields = split_csv(line);
        if fields.len() < 3 {
            continue;
        }
        let name = fields[1].trim();
        // Header (`tag_id,name,category,count`) or an unparseable category row is skipped.
        let Ok(category) = fields[2].trim().parse::<u8>() else { continue };
        if name.is_empty() {
            continue;
        }
        let count = fields.get(3).and_then(|c| c.trim().parse::<u32>().ok()).unwrap_or(0);
        tags.push(Wd14Tag { name: name.to_string(), category, count });
    }
    if tags.is_empty() {
        return Err(Error::EmptyTags);
    }
    Ok(tags)
}

/// An opened tagger pack. The tag list is kept parsed; the context binary is mmapped per run.
pub struct Wd14Pack {
    dir: PathBuf,
    tags: Vec<Wd14Tag>,
}

impl Wd14Pack {
    /// Validate `dir` and parse `tags.csv`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotWd14Pack(dir));
        }
        for name in REQUIRED {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        let tags = parse_tags_csv(&std::fs::read_to_string(dir.join("tags.csv"))?)?;
        Ok(Self { dir, tags })
    }

    /// True when `dir` carries the `WD14` marker.
    pub fn is_wd14_pack(dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(MARKER).exists()
    }

    /// The pack directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The classifier's tags, in logit order.
    pub fn tags(&self) -> &[Wd14Tag] {
        &self.tags
    }

    /// Path of `name` inside the pack.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn model_bin(&self) -> PathBuf {
        self.path("model.bin")
    }

    /// mmap the context binary.
    pub fn map(&self, name: &str) -> Result<Mmap> {
        let f = std::fs::File::open(self.path(name))?;
        Ok(unsafe { Mmap::map(&f)? })
    }
}

impl std::fmt::Debug for Wd14Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wd14Pack").field("dir", &self.dir).field("tags", &self.tags.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_rejected() {
        let dir = std::env::temp_dir().join("local-wd14-pack-empty");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(MARKER));
        assert!(matches!(Wd14Pack::open(&dir), Err(Error::NotWd14Pack(_))));
        assert!(!Wd14Pack::is_wd14_pack(&dir));
    }

    #[test]
    fn marker_without_files_reports_the_missing_one() {
        let dir = std::env::temp_dir().join("local-wd14-pack-marker");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert!(Wd14Pack::is_wd14_pack(&dir));
        match Wd14Pack::open(&dir) {
            Err(Error::MissingFile(p)) => assert!(p.ends_with("model.bin")),
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_header_and_categories_in_order() {
        let csv = "tag_id,name,category,count\n\
            9999999,general,9,100\n\
            1,1girl,0,5000000\n\
            2,solo,0,4000000\n\
            3,hakurei_reimu,4,90000\n";
        let tags = parse_tags_csv(csv).unwrap();
        assert_eq!(tags.len(), 4);
        assert_eq!(tags[0], Wd14Tag { name: "general".into(), category: 9, count: 100 });
        assert_eq!(tags[1].name, "1girl");
        assert_eq!(tags[1].category, CATEGORY_GENERAL);
        assert_eq!(tags[3].category, CATEGORY_CHARACTER);
        // Underscores fold to spaces for prompt insertion.
        assert_eq!(tags[3].insert_text(), "hakurei reimu");
    }

    #[test]
    fn quoted_names_survive_and_empty_is_rejected() {
        let csv = "1,\"tag,with,commas\",0,10\n";
        let tags = parse_tags_csv(csv).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "tag,with,commas");
        assert!(matches!(parse_tags_csv("tag_id,name,category,count\n"), Err(Error::EmptyTags)));
    }
}
