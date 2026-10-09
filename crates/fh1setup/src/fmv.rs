//! Setup group `fmv`: FH1's movies (data/extracted/plans/fmv.md, architecture.md §3a): the original WMV + .def files
//! copied byte-for-byte (no transcode; ffmpeg decodes them at runtime), subtitle XML -> JSON, `manifest.json`.
//!
//! Output (group dir): `FMV_0N.wmv/.def`, `PressStart.wmv/.def`, `fmv_0N_subtitles.json`
//! (`{ "<LANG>": [ {key, index, start, text_id, extra_attrs}, ... ] }`), `manifest.json` (`[{path, bytes, source}]`).
//! Switching to a transcoded container later: change [`EXT`] (and the copy in [`movies`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Movie container extension, on the disc and in the output. One place to change.
const EXT: &str = "wmv";
/// Disc directory (under `media/`) holding the videos.
const VIDEO_DIR: &str = "ui/videos";

/// (output name, disc path under `media/ui/videos` without extension, has a .def sidecar).
/// Only the movies the engine plays: `splash_intros/` (T10_MS_Combined, Dolby_Corona_Intro, Forza_Intro, Forza_Tone,
/// MSGS_Logo) is not copied (user, 2026-10-09: no splash screens; the other three have no known trigger).
const MOVIES: &[(&str, &str, bool)] = &[
    ("FMV_01", "FMV_01", true),
    ("FMV_02", "FMV_02", true),
    ("FMV_04", "FMV_04", true),
    ("PressStart", "PressStart", true),
];

/// Movies that carry a `fmv_0N_subtitles/<LANG>.xml` folder.
const SUBTITLED: &[&str] = &["FMV_01", "FMV_02", "FMV_04"];

/// One subtitle cue: `<SubtitleN Start="secs" [other attrs]/>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub key: String,
    pub index: u32,
    pub start: f64,
    pub extra_attrs: Option<BTreeMap<String, String>>,
}

impl Cue {
    pub fn to_json(&self) -> Value {
        let extra = match &self.extra_attrs {
            Some(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()),
            None => Value::Null,
        };
        json!({ "key": self.key, "index": self.index, "start": self.start, "text_id": Value::Null, "extra_attrs": extra })
    }
}

struct Entry {
    path: String,
    bytes: u64,
    source: String,
}

/// Parse a subtitle XML file: children of the root, each `<SubtitleN Start=".."/>`, sorted by index.
pub fn parse_subtitles(xml: &str) -> Result<Vec<Cue>> {
    let xml = xml.trim_start_matches('\u{feff}');
    let mut cues = Vec::new();
    let mut root_seen = false;
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        let gt = after.find('>').context("unterminated tag in subtitle XML")?;
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let tag = tag.trim_end_matches('/').trim();
        let (name, attrs_src) = match tag.find(char::is_whitespace) {
            Some(i) => (&tag[..i], &tag[i..]),
            None => (tag, ""),
        };
        if !root_seen {
            root_seen = true; // the root element (<Subtitles>)
            continue;
        }
        let mut attrs = parse_attrs(attrs_src)?;
        let start_s = attrs.remove("Start").with_context(|| format!("<{name}> has no Start attribute"))?;
        let start: f64 = start_s.trim().parse().with_context(|| format!("<{name}> bad Start {start_s:?}"))?;
        if !start.is_finite() {
            bail!("<{name}> non-finite Start");
        }
        let index: u32 = name
            .replace("Subtitle", "")
            .parse()
            .with_context(|| format!("subtitle element name {name:?} is not SubtitleN"))?;
        cues.push(Cue { key: name.to_string(), index, start, extra_attrs: if attrs.is_empty() { None } else { Some(attrs) } });
    }
    cues.sort_by_key(|c| c.index);
    Ok(cues)
}

fn parse_attrs(mut s: &str) -> Result<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return Ok(m);
        }
        let eq = s.find('=').context("attribute without '='")?;
        let name = s[..eq].trim().to_string();
        let v = s[eq + 1..].trim_start();
        let q = v.chars().next().filter(|c| *c == '"' || *c == '\'').context("attribute value not quoted")?;
        let v = &v[1..];
        let end = v.find(q).context("unterminated attribute value")?;
        m.insert(name, v[..end].to_string());
        s = &v[end + 1..];
    }
}

/// `{ "<LANG>": [cue json...] }` from language -> xml text (keys sorted by language).
pub fn subtitles_json(xml_by_lang: &BTreeMap<String, String>) -> Result<Value> {
    let mut obj = serde_json::Map::new();
    for (lang, xml) in xml_by_lang {
        let cues = parse_subtitles(xml).with_context(|| format!("subtitles {lang}"))?;
        obj.insert(lang.clone(), Value::Array(cues.iter().map(Cue::to_json).collect()));
    }
    Ok(Value::Object(obj))
}

/// Case-insensitive lookup of `rel` (forward-slash separated) under `base`.
fn find_ci(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut cur = base.to_path_buf();
    for comp in rel.split('/') {
        let direct = cur.join(comp);
        if direct.exists() {
            cur = direct;
            continue;
        }
        let mut found = None;
        for e in std::fs::read_dir(&cur).with_context(|| format!("reading {}", cur.display()))? {
            let e = e?;
            if e.file_name().to_string_lossy().eq_ignore_ascii_case(comp) {
                found = Some(e.path());
                break;
            }
        }
        cur = found.with_context(|| format!("{comp} not found in {}", cur.display()))?;
    }
    Ok(cur)
}

fn video_root(disc: &Path) -> Result<PathBuf> {
    find_ci(disc, &format!("media/{VIDEO_DIR}"))
}

fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))
}

/// Copy every movie (and .def sidecar). Fails if any is missing from the disc.
fn movies(disc: &Path, out: &Path) -> Result<Vec<Entry>> {
    let root = video_root(disc)?;
    let mut entries = Vec::new();
    for &(name, src_rel, has_def) in MOVIES {
        let mut files = vec![(format!("{src_rel}.{EXT}"), format!("{name}.{EXT}"))];
        if has_def {
            files.push((format!("{src_rel}.def"), format!("{name}.def")));
        }
        for (src_rel, dst_rel) in files {
            let src = find_ci(&root, &src_rel).with_context(|| format!("FH1 disc is missing media/{VIDEO_DIR}/{src_rel}"))?;
            let bytes = copy_file(&src, &out.join(&dst_rel))?;
            entries.push(Entry { path: dst_rel, bytes, source: format!("media/{VIDEO_DIR}/{src_rel}") });
        }
    }
    Ok(entries)
}

/// Build `fmv_0N_subtitles.json` for each subtitled movie. Returns manifest entries and the language count.
fn subtitles(disc: &Path, out: &Path) -> Result<(Vec<Entry>, usize)> {
    let root = video_root(disc)?;
    let mut entries = Vec::new();
    let mut langs = 0;
    std::fs::create_dir_all(out)?;
    for fmv in SUBTITLED {
        let folder_name = format!("{}_subtitles", fmv.to_lowercase());
        let folder = find_ci(&root, &folder_name).with_context(|| format!("FH1 disc is missing {folder_name}"))?;
        let mut xml_by_lang = BTreeMap::new();
        for e in std::fs::read_dir(&folder)? {
            let p = e?.path();
            if p.extension().map_or(false, |x| x.to_string_lossy().eq_ignore_ascii_case("xml")) {
                let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
                let bytes = std::fs::read(&p)?;
                xml_by_lang.insert(stem, String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        if xml_by_lang.is_empty() {
            bail!("{} holds no subtitle XML", folder.display());
        }
        langs = langs.max(xml_by_lang.len());
        let v = subtitles_json(&xml_by_lang)?;
        let name = format!("{folder_name}.json");
        let mut text = serde_json::to_string_pretty(&v)?;
        text.push('\n');
        std::fs::write(out.join(&name), &text)?;
        entries.push(Entry { path: name, bytes: text.len() as u64, source: format!("media/{VIDEO_DIR}/{folder_name}/*.xml") });
    }
    Ok((entries, langs))
}

fn write_manifest(out: &Path, mut entries: Vec<Entry>) -> Result<()> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let v: Vec<Value> = entries.iter().map(|e| json!({ "path": e.path, "bytes": e.bytes, "source": e.source })).collect();
    let mut text = serde_json::to_string_pretty(&v)?;
    text.push('\n');
    std::fs::write(out.join("manifest.json"), text)?;
    Ok(())
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let mut entries = movies(disc, out)?;
    let dot_ext = format!(".{EXT}");
    let movie_bytes: u64 = entries.iter().filter(|e| e.path.ends_with(&dot_ext)).map(|e| e.bytes).sum();
    let n_movies = entries.iter().filter(|e| e.path.ends_with(&dot_ext)).count();
    let n_def = entries.iter().filter(|e| e.path.ends_with(".def")).count();
    let (subs, langs) = subtitles(disc, out)?;
    let n_subs = subs.len();
    entries.extend(subs);
    write_manifest(out, entries)?;
    println!("  fmv: {n_movies} movies ({} MB), {n_def} .def, {n_subs} subtitle sets ({langs} languages)", movie_bytes / 1_000_000);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FMV04_EN: &str = r#"<Subtitles>
  <Subtitle0 Start="5.527000"/>
  <Subtitle1 Start="9.300000"/>
  <Subtitle2 Start="12.46000"/>
  <Subtitle3 Start="14.650000"/>
  <Subtitle4 Start="18.200000"/>
  <Subtitle5 Start="23.700000"/>
  <Subtitle6 Start="30.150000"/>
  <Subtitle7 Start="39.900000"/>
  <Subtitle8 Start="44.600000"/>
  <Subtitle9 Start="47.550000"/>
</Subtitles>"#;

    #[test]
    fn parses_fmv04_en() {
        let cues = parse_subtitles(FMV04_EN).unwrap();
        assert_eq!(cues.len(), 10);
        assert_eq!(cues[0].start, 5.527);
        assert_eq!(cues[9].start, 47.55);
        for (i, c) in cues.iter().enumerate() {
            assert_eq!(c.index as usize, i);
            assert_eq!(c.key, format!("Subtitle{i}"));
            assert!(c.extra_attrs.is_none());
        }
        let j = cues[0].to_json();
        assert_eq!(j["text_id"], Value::Null);
        assert_eq!(j["start"], json!(5.527));
    }

    #[test]
    fn extra_attrs_kept() {
        let cues = parse_subtitles(r#"<Subtitles><Subtitle1 Start="2" Dur='3'/><Subtitle0 Start="1"/></Subtitles>"#).unwrap();
        assert_eq!(cues[0].index, 0);
        assert_eq!(cues[1].extra_attrs.as_ref().unwrap()["Dur"], "3");
    }

    fn disc_root() -> Option<PathBuf> {
        let root = std::env::var_os("FH1_DISC")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
        root.join("media").is_dir().then_some(root)
    }

    #[test]
    fn disc_subtitles_match_golden() {
        let Some(disc) = disc_root() else {
            eprintln!("fmv: disc absent, skipping");
            return;
        };
        let out = std::env::temp_dir().join(format!("fh1_fmv_test_{}", std::process::id()));
        let (entries, langs) = subtitles(&disc, &out).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(langs, 21);
        for (n, cues) in [("01", 18usize), ("02", 16), ("04", 10)] {
            let v: Value = serde_json::from_slice(&std::fs::read(out.join(format!("fmv_{n}_subtitles.json"))).unwrap()).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 21);
            for (_, arr) in obj {
                assert_eq!(arr.as_array().unwrap().len(), cues);
            }
            let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../data/extracted/converted/ui/videos/fmv_{n}_subtitles.json"));
            if golden.exists() {
                let g: Value = serde_json::from_slice(&std::fs::read(golden).unwrap()).unwrap();
                assert_eq!(v, g, "fmv_{n} subtitles differ from golden");
            }
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn disc_defs_present() {
        let Some(disc) = disc_root() else {
            eprintln!("fmv: disc absent, skipping");
            return;
        };
        let root = video_root(&disc).unwrap();
        for n in ["FMV_01", "FMV_02", "FMV_04", "PressStart"] {
            assert!(find_ci(&root, &format!("{n}.def")).is_ok(), "{n}.def");
            assert!(find_ci(&root, &format!("{n}.{EXT}")).is_ok(), "{n}.{EXT}");
        }
        let d = std::fs::read_to_string(find_ci(&root, "FMV_01.def").unwrap()).unwrap();
        assert!(d.contains("Video id=\"7\""));
    }
}
