//! Every `FH1_*` flag, documented and settable in `data/settings.json` (section `flags`).
//!
//! The table is generated at build time (build.rs scans `crates/*/src` for the variables the game reads; the text comes
//! from `src/flag_docs.tsv`, or the code comment when a flag has no curated entry). At startup, before anything reads
//! the environment ([`apply_startup`], top of `main`):
//! 1. every `flags.<NAME>.value` that is not `null` is set as the environment variable `NAME`, unless the real
//!    environment (launch.bat, the launcher, a shell) already sets it: that one wins and the entry shows `"launcher"`;
//! 2. the section is rewritten so it always lists every current flag (new flags appear with `value: null`, the player's
//!    values are kept, flags the game no longer reads are dropped unless they still hold a value, then they are kept
//!    with area "Stale").
//! The pause menu's save (ui.rs `Settings`) writes the same section back, so nothing is lost.
//! Setup-only (fh1setup) and launcher-only variables are not listed; test hooks are listed under "Dev hooks".

#![allow(dead_code)] // the table carries fields only some readers use (AREAS, curated)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Map, Value};

/// One flag of the generated table.
pub struct Flag {
    pub name: &'static str,
    /// One of [`AREAS`].
    pub area: &'static str,
    /// Behaviour when unset: "on" (`0` turns it off), "off" (`1` turns it on), a number, or "unset".
    pub default: &'static str,
    pub what: &'static str,
    pub effect: &'static str,
    /// `crate/src/file.rs:line` of a read.
    pub location: &'static str,
    /// The text is from src/flag_docs.tsv (else from the code comment).
    pub curated: bool,
}

include!(concat!(env!("OUT_DIR"), "/flags_gen.rs"));

pub const HELP: &str = "Every FH1_* flag of the game is listed under \"flags\", grouped by area. Each entry: \"value\" = null uses \
the built-in default (nothing is set); any other value (\"0\", \"1\", a number or a word, as text or a JSON number/bool) is applied \
as that environment variable at the next start. \"default\" is what happens when it is null: \"on\" = a feature/fix that is on \
(set \"0\" to turn it off and get the old behaviour), \"off\" = opt-in (set \"1\" to turn it on), a number = the built-in value \
of a tuning knob, \"unset\" = no value (see \"what\"). \"what\" says what it does, \"effect\" what you see or feel. Every change needs \
a restart. If launch.bat or the launcher sets the same variable, that wins and the entry shows it as \"launcher\". Flags in \
\"Dev hooks\" are for testing (screenshots, autodrive, tours) and can quit or break normal play. The game rewrites this section \
on every start and save: descriptions are refreshed, your values are kept.";

/// `FH1_*` variables set in the real environment before settings.json was applied (name -> value).
static LAUNCHER: OnceLock<HashMap<String, String>> = OnceLock::new();
/// What [`apply_startup`] did, logged by [`log_startup`] once logging is up.
static REPORT: OnceLock<Vec<String>> = OnceLock::new();

/// serde: the `flags` section, any malformed shape = empty (never fails the whole settings file, which would reset
/// every option to its default on the next save).
pub fn de_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Map<String, Value>, D::Error> {
    use serde::Deserialize;
    Ok(match Value::deserialize(d)? {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

/// serde: `_flags_help`, any non-string = empty (it is rewritten anyway).
pub fn de_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize;
    Ok(Value::deserialize(d)?.as_str().unwrap_or_default().to_string())
}

/// A settings.json value as the environment string: null / arrays / objects = not set.
pub fn value_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.into()),
        _ => None,
    }
}

fn data_dir_from_args() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--data" {
            if let Some(d) = args.next() {
                return d.into();
            }
        }
    }
    PathBuf::from("data")
}

/// Top of `main`, single-threaded, before anything reads an `FH1_*` variable (most cache it in a OnceLock): applies
/// settings.json `flags` values and rewrites the section. (Edition 2021: `set_var` is safe; it must still run before
/// any thread starts, which is why it is the first call in `main`.)
pub fn apply_startup() {
    let launcher: HashMap<String, String> =
        std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).filter(|(k, _)| k.starts_with("FH1_")).collect();
    let path = data_dir_from_args().join("settings.json");
    let mut report = Vec::new();
    let root: Option<Value> = match std::fs::read(&path) {
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) if v.is_object() => Some(v),
            Ok(_) | Err(_) => {
                // Leave a broken file alone (the menu's own loader falls back to defaults too).
                report.push(format!("flags: {} is not a JSON object; flags not applied and the file is left as is", path.display()));
                None
            }
        },
        Err(_) => Some(Value::Object(Map::new())),
    };
    let _ = LAUNCHER.set(launcher.clone());
    let Some(mut root) = root else {
        let _ = REPORT.set(report);
        return;
    };
    let old = root.get("flags").and_then(Value::as_object).cloned().unwrap_or_default();
    let (mut applied, mut skipped) = (0, 0);
    for (name, entry) in &old {
        if !name.starts_with("FH1_") || name.contains(['=', '\0']) {
            continue;
        }
        let Some(val) = entry.get("value").and_then(value_string) else { continue };
        if val.contains('\0') {
            continue;
        }
        if launcher.contains_key(name) {
            skipped += 1;
            report.push(format!("flags: {name}={val} from settings.json ignored: set by the launcher/environment to {:?}", launcher[name]));
            continue;
        }
        std::env::set_var(name, &val);
        applied += 1;
        let dev = FLAGS.iter().any(|f| f.name == name && f.area == "Dev hooks");
        report.push(format!("flags: {name}={val} (settings.json){}", if dev { "  [DEV HOOK]" } else { "" }));
    }
    report.push(format!("flags: {applied} applied from settings.json, {skipped} overridden by the environment, {} known flags", FLAGS.len()));
    let _ = REPORT.set(report);

    // Rewrite the section (only when it changed, and only when the file exists or there is something to write).
    let obj = root.as_object_mut().expect("checked above");
    let new_flags = section(Some(&old));
    let changed = obj.get("flags").and_then(Value::as_object) != Some(&new_flags) || obj.get("_flags_help").and_then(Value::as_str) != Some(HELP);
    if changed {
        obj.insert("_flags_help".into(), Value::String(HELP.into()));
        obj.insert("flags".into(), Value::Object(new_flags));
        write_file(&path, &root);
    }
}

/// After logging is set up: what [`apply_startup`] did.
pub fn log_startup() {
    for line in REPORT.get().map(Vec::as_slice).unwrap_or(&[]) {
        bevy::log::info!("{line}");
    }
}

fn write_file(path: &Path, v: &Value) {
    let Ok(bytes) = serde_json::to_vec_pretty(v) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Atomic: temp file + rename, so a crash never leaves half a settings file.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::write(path, &bytes);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The full `flags` section in table order (area, then name), keeping the player's `value`s from `old`.
/// (serde_json `preserve_order` is on for this crate, so the file keeps this order.)
pub fn section(old: Option<&Map<String, Value>>) -> Map<String, Value> {
    let launcher = LAUNCHER.get();
    let old_value = |name: &str| old.and_then(|o| o.get(name)).and_then(|e| e.get("value")).cloned().filter(|v| !v.is_null());
    let mut out = Map::new();
    for f in FLAGS {
        let mut e = Map::new();
        e.insert("value".into(), old_value(f.name).unwrap_or(Value::Null));
        e.insert("default".into(), Value::String(f.default.into()));
        e.insert("area".into(), Value::String(f.area.into()));
        e.insert("what".into(), Value::String(f.what.into()));
        e.insert("effect".into(), Value::String(f.effect.into()));
        e.insert("restart".into(), Value::Bool(true));
        if let Some(v) = launcher.and_then(|l| l.get(f.name)) {
            e.insert("launcher".into(), Value::String(v.clone()));
        }
        e.insert("where".into(), Value::String(f.location.into()));
        out.insert(f.name.into(), Value::Object(e));
    }
    // Flags no longer read by the game: kept only while they hold a value, so the player sees it is unused.
    if let Some(old) = old {
        for (name, entry) in old {
            if out.contains_key(name) {
                continue;
            }
            let Some(v) = entry.get("value").filter(|v| !v.is_null()) else { continue };
            let mut e = Map::new();
            e.insert("value".into(), v.clone());
            e.insert("default".into(), Value::String("unset".into()));
            e.insert("area".into(), Value::String("Stale".into()));
            e.insert("what".into(), Value::String("No longer read by this version of the game (still applied as an environment variable). Set value to null to remove it.".into()));
            e.insert("effect".into(), Value::String("None that is known.".into()));
            e.insert("restart".into(), Value::Bool(true));
            out.insert(name.clone(), Value::Object(e));
        }
    }
    out
}
