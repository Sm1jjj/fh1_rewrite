//! Build-time flag registry (src/flags.rs): every `FH1_*` environment variable the game reads.
//!
//! Scans `crates/*/src/**/*.rs` (not fh1setup / fh1-launcher: setup-only or launcher-only variables never reach the
//! game) for whole string literals `"FH1_…"` that are read (not `.env(..)` / `set_var` writers, not comments), and
//! writes `$OUT_DIR/flags_gen.rs` = `static FLAGS: &[Flag]`, sorted by area then name. For each flag:
//! - the curated text from `src/flag_docs.tsv` (`NAME<TAB>AREA<TAB>DEFAULT<TAB>WHAT<TAB>EFFECT`, `#` comments), else
//! - a fallback: area from the file path, default inferred from the read pattern (`map_or(true, |v| v != "0")` = on,
//!   `== "1"` = off, `parse` + `unwrap_or(x)` = x, otherwise unset) and the nearest comment mentioning the flag.
//! Never panics on odd input: unreadable files are skipped, and a failed scan still writes an (empty) table.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Display order of the areas (src/flags.rs sorts by this, and the settings.json section follows it).
const AREAS: &[&str] = &[
    "Renderer",
    "Car reflections",
    "Static world",
    "Scenery/props/zones",
    "Shadows/post/sky",
    "Car paint/models",
    "Vehicle/handling",
    "Drivetrain/assists",
    "AI/traffic/races",
    "Audio/radio",
    "Effects",
    "UI/HUD",
    "Perf/logging",
    "Net",
    "Other",
    "Dev hooks",
];

/// Test / automation hooks: always in "Dev hooks" (they exit the game, take screenshots, drive, teleport, ...).
const DEV_PREFIXES: &[&str] = &[
    "FH1_SHOT",
    "FH1_BAKE",
    "FH1_MAP_TOUR",
    "FH1_PERF_TOUR",
    "FH1_AUTODRIVE",
    "FH1_TELEPORT",
    "FH1_RACE_AUTOFINISH",
    "FH1_XEX_KEY",
    "FH1_PHOTO_",
    "FH1_MENU",
    "FH1_UI_SCENE",
    "FH1_UI_EVENTS",
    "FH1_UI_SLIDES",
];

/// Crates whose variables are not game flags.
const SKIP_CRATES: &[&str] = &["fh1setup", "fh1-launcher"];

#[derive(Default, Clone)]
struct Found {
    location: String,
    default: String,
    comment: String,
    area: String,
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let crates_dir = manifest.parent().map(Path::to_path_buf).unwrap_or_else(|| manifest.clone());
    let docs_path = manifest.join("src").join("flag_docs.tsv");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", docs_path.display());

    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&crates_dir) {
        let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if SKIP_CRATES.contains(&name.as_str()) {
                continue;
            }
            let src = d.join("src");
            if src.is_dir() {
                println!("cargo:rerun-if-changed={}", src.display());
                walk(&src, &name, &mut files);
            }
        }
    }

    // Pass 1: file texts, helper functions (fn taking a name and reading the environment), comment mentions.
    let mut texts = Vec::new();
    for (krate, path) in &files {
        if let Ok(t) = std::fs::read_to_string(path) {
            let rel = path.strip_prefix(&crates_dir).unwrap_or(path).to_string_lossy().replace('\\', "/");
            let _ = krate;
            texts.push((rel, t));
        }
    }
    let mut helpers_global: HashMap<String, String> = HashMap::new();
    let mut helpers_local: Vec<HashMap<String, String>> = Vec::new();
    let mut comment_of: HashMap<String, String> = HashMap::new();
    for (_, t) in &texts {
        let lines: Vec<&str> = t.lines().collect();
        let h = helpers(&lines);
        for (k, v) in &h {
            helpers_global.entry(k.clone()).or_insert_with(|| v.clone());
        }
        helpers_local.push(h);
        for l in &lines {
            let s = l.trim_start();
            if let Some(c) = s.strip_prefix("//") {
                let c = c.trim_start_matches(['/', '!']).trim();
                for name in names_in(c) {
                    comment_of.entry(name).or_insert_with(|| clean_comment(c));
                }
            } else if let Some(i) = l.find("// ") {
                // Trailing comment.
                let c = l[i + 3..].trim();
                for name in names_in(c) {
                    comment_of.entry(name).or_insert_with(|| clean_comment(c));
                }
            }
        }
    }

    // Pass 2: reads.
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for (fi, (rel, t)) in texts.iter().enumerate() {
        let lines: Vec<&str> = t.lines().collect();
        for (li, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || !line.contains("\"FH1_") {
                continue;
            }
            if line.contains(".env(") || line.contains("set_var") || line.contains("remove_var") {
                continue;
            }
            for (pos, name) in literals(line) {
                let before = &line[..pos];
                let after = &line[pos + name.len() + 2..];
                // Statement after the literal: up to ';' over the next lines, at most 400 chars.
                let mut stmt = String::from(after);
                let mut k = li + 1;
                while !stmt.contains(';') && stmt.len() < 400 && k < lines.len() && k < li + 6 {
                    stmt.push(' ');
                    stmt.push_str(lines[k].trim());
                    k += 1;
                }
                if let Some(i) = stmt.find(';') {
                    stmt.truncate(i);
                }
                let callee = callee(before);
                let helper = match callee.as_str() {
                    "var" | "var_os" | "" => None,
                    c => helpers_local[fi].get(c).or_else(|| helpers_global.get(c)),
                };
                let default = match helper {
                    Some(body) => classify(body, &stmt, true),
                    None => classify(&stmt, &stmt, false),
                };
                let comment = comment_of.get(&name).cloned().unwrap_or_else(|| doc_above(&lines, li));
                let entry = found.entry(name.clone()).or_insert_with(|| Found {
                    location: format!("{rel}:{}", li + 1),
                    default: default.clone(),
                    comment: comment.clone(),
                    area: area_for(&name, rel),
                });
                // Prefer a classified read over an unknown first sighting (e.g. a name in a table).
                if entry.default == "unset" && default != "unset" {
                    entry.default = default;
                    entry.location = format!("{rel}:{}", li + 1);
                }
            }
        }
    }

    // Curated docs.
    let mut docs: HashMap<String, [String; 4]> = HashMap::new();
    if let Ok(t) = std::fs::read_to_string(&docs_path) {
        for l in t.lines() {
            if l.trim().is_empty() || l.starts_with('#') {
                continue;
            }
            let c: Vec<&str> = l.split('\t').map(str::trim).collect();
            if c.len() >= 5 && c[0].starts_with("FH1_") {
                docs.insert(c[0].to_string(), [c[1].to_string(), c[2].to_string(), c[3].to_string(), c[4].to_string()]);
            }
        }
    }

    let mut rows: Vec<(usize, String, String)> = Vec::new();
    for (name, f) in &found {
        let dev = DEV_PREFIXES.iter().any(|p| name.starts_with(p));
        let (area, default, what, effect, curated) = match docs.get(name) {
            Some([a, d, w, e]) => {
                let a = if dev { "Dev hooks".to_string() } else if AREAS.contains(&a.as_str()) { a.clone() } else { f.area.clone() };
                let d = if d.is_empty() { f.default.clone() } else { d.clone() };
                (a, d, w.clone(), e.clone(), true)
            }
            None => {
                let what = if f.comment.is_empty() { format!("Undocumented flag; read at {}.", f.location) } else { f.comment.clone() };
                let effect = if dev {
                    "Testing only: a development hook; leave it unset for normal play.".to_string()
                } else {
                    "Not described yet (text taken from the code comment).".to_string()
                };
                (f.area.clone(), f.default.clone(), what, effect, false)
            }
        };
        let idx = AREAS.iter().position(|a| *a == area).unwrap_or(AREAS.len() - 2);
        let mut s = String::new();
        let _ = write!(
            s,
            "    Flag {{ name: {:?}, area: {:?}, default: {:?}, what: {:?}, effect: {:?}, location: {:?}, curated: {} }},\n",
            name, AREAS[idx], default, what, effect, f.location, curated
        );
        rows.push((idx, name.clone(), s));
    }
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let mut out = String::from("// @generated by crates/fh1-engine/build.rs: every FH1_* variable the game reads.\n");
    out.push_str("pub static AREAS: &[&str] = &[");
    for a in AREAS {
        let _ = write!(out, "{a:?}, ");
    }
    out.push_str("];\n");
    out.push_str("pub static FLAGS: &[Flag] = &[\n");
    for (_, _, s) in &rows {
        out.push_str(s);
    }
    out.push_str("];\n");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into()));
    let dest = out_dir.join("flags_gen.rs");
    if std::fs::read_to_string(&dest).ok().as_deref() != Some(out.as_str()) {
        let _ = std::fs::write(&dest, out);
    }
}

fn walk(dir: &Path, krate: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') {
                walk(&p, krate, out);
            }
        } else if name.ends_with(".rs") {
            out.push((krate.to_string(), p));
        }
    }
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_'
}

/// Whole string literals `"FH1_NAME"` in a line: (byte offset of the opening quote, name).
fn literals(line: &str) -> Vec<(usize, String)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = line[i..].find("\"FH1_") {
        let start = i + off;
        let mut j = start + 1;
        while j < b.len() && is_name_char(b[j]) {
            j += 1;
        }
        if j < b.len() && b[j] == b'"' && j > start + 5 {
            let name = &line[start + 1..j];
            if !name.ends_with('_') {
                out.push((start, name.to_string()));
            }
        }
        i = j.max(start + 1);
        if i >= line.len() {
            break;
        }
    }
    out
}

/// `FH1_` names mentioned anywhere in a text (comments).
fn names_in(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = s[i..].find("FH1_") {
        let start = i + off;
        let prev_ok = start == 0 || !is_name_char(b[start - 1]);
        let mut j = start + 4;
        while j < b.len() && is_name_char(b[j]) {
            j += 1;
        }
        if prev_ok && j > start + 4 {
            let n = s[start..j].trim_end_matches('_');
            if n.len() > 4 {
                out.push(n.to_string());
            }
        }
        i = j.max(start + 1);
        if i >= s.len() {
            break;
        }
    }
    out
}

fn clean_comment(c: &str) -> String {
    let c = c.replace('`', "");
    let mut s: String = c.trim().chars().take(220).collect();
    if c.chars().count() > 220 {
        s.push_str("...");
    }
    s
}

/// The identifier just before the literal's `(`: `var` in `std::env::var("..")`, `flag_on` in `flag_on("..")`.
fn callee(before: &str) -> String {
    let t = before.trim_end();
    let Some(t) = t.strip_suffix('(') else { return String::new() };
    let t = t.trim_end();
    let id: String = t.chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    id.chars().rev().collect()
}

/// Functions whose body reads the environment through a parameter (no FH1 literal of their own): name -> body.
fn helpers(lines: &[&str]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(p) = l.find("fn ") else { continue };
        if l.trim_start().starts_with("//") {
            continue;
        }
        let rest = &l[p + 3..];
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if name.is_empty() || !rest[name.len()..].starts_with('(') || !rest.contains("&str") {
            continue;
        }
        let mut body = String::new();
        let mut depth = 0i32;
        let mut opened = false;
        for l2 in lines.iter().skip(i).take(40) {
            body.push_str(l2.trim());
            body.push(' ');
            for c in l2.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if opened && depth <= 0 {
                break;
            }
        }
        if (body.contains("env::var") || body.contains("var_os(")) && !body.contains("\"FH1_") {
            out.insert(name, body);
        }
    }
    out
}

/// Default semantics: "on" (FH1_X=0 turns it off), "off" (opt-in, FH1_X=1), a number, or "unset".
fn classify(code: &str, call: &str, helper: bool) -> String {
    if code.contains("parse") {
        // Default: the helper call's 2nd argument, else `unwrap_or(x)`.
        let tok = if helper { call.trim_start().strip_prefix(',').map(first_arg) } else { None };
        let tok = tok.or_else(|| code.find("unwrap_or(").map(|i| first_arg(&code[i + 10..])));
        if let Some(n) = tok.and_then(|t| number(&t)) {
            return n;
        }
        return "unset".into();
    }
    if helper {
        let arg = call.trim_start().strip_prefix(',').map(first_arg).unwrap_or_default();
        if arg == "false" {
            return "off".into();
        }
        if arg == "true" {
            return "on".into();
        }
    }
    if code.contains("\"0\"") {
        return "on".into();
    }
    if code.contains("\"1\"") || code.contains(".is_some()") || code.contains(".is_ok()") {
        return "off".into();
    }
    "unset".into()
}

fn first_arg(s: &str) -> String {
    s.trim().chars().take_while(|c| *c != ',' && *c != ')').collect::<String>().trim().to_string()
}

/// `0.8f32` / `16` / `1_000` / `-1.5` -> a plain number string.
fn number(t: &str) -> Option<String> {
    let t = t.replace('_', "");
    let t = ["f32", "f64", "u8", "u16", "u32", "u64", "usize", "i32", "i64", "isize"].iter().fold(t, |t, s| t.trim_end_matches(*s).to_string());
    t.parse::<f64>().ok().map(|_| t)
}

/// `///` doc lines above the function containing line `li`.
fn doc_above(lines: &[&str], li: usize) -> String {
    let mut i = li;
    while i > 0 && !lines[i].contains("fn ") {
        i -= 1;
        if li - i > 60 {
            return String::new();
        }
    }
    let mut doc = Vec::new();
    let mut j = i;
    while j > 0 {
        j -= 1;
        let s = lines[j].trim_start();
        if let Some(d) = s.strip_prefix("///") {
            doc.push(d.trim());
        } else if s.starts_with("#[") {
            continue;
        } else {
            break;
        }
    }
    doc.reverse();
    clean_comment(&doc.join(" "))
}

fn area_for(name: &str, rel: &str) -> String {
    if DEV_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return "Dev hooks".into();
    }
    let has = |k: &str| rel.contains(k);
    let a = if rel.starts_with("fh1-audio") || rel.starts_with("fh1-radio") || has("/audio") || has("/radio") {
        "Audio/radio"
    } else if has("car_probe") || has("reflect") {
        "Car reflections"
    } else if has("static_world") {
        "Static world"
    } else if has("drivetrain") || has("assist") {
        "Drivetrain/assists"
    } else if has("/vehicle") || has("/data.rs") || has("tyre") {
        "Vehicle/handling"
    } else if has("/ai") || has("traffic") || has("/race") || has("progression") {
        "AI/traffic/races"
    } else if has("effects") || has("smoke") || has("backfire") || has("skidmarks") || has("particles") {
        "Effects"
    } else if has("shadow") || has("post") || has("sky") || has("light") || has("tod") || has("cloud") || has("exposure") {
        "Shadows/post/sky"
    } else if has("scenery") || has("props") || has("zone") || has("grass") || has("crowd") || has("objects") || has("smash") || has("anim") || has("pvs") || has("world") {
        "Scenery/props/zones"
    } else if has("/car") || has("wheel") || has("paint") {
        "Car paint/models"
    } else if has("/ui") {
        "UI/HUD"
    } else if has("perf") || has("diag") || has("logpipe") {
        "Perf/logging"
    } else if has("/net") || rel.starts_with("fh1-net") {
        "Net"
    } else if rel.starts_with("fh1-remaster") || rel.starts_with("fh1-render") || rel.starts_with("fh1-shaders") {
        "Renderer"
    } else {
        "Other"
    };
    a.into()
}
