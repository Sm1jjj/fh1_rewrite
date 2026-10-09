//! Colorado soundscape tiles (`bin.zip/*.soundscape`) + the ambience/reverb/soundbank-lookup
//! XML → JSON: one file per tile, `index.json`, `_templates/{ambience,reverb,soundbank_lookup}.json`.
//!
//! Port of the staging converter; schema identical except that **Z is negated** at setup for every
//! coordinate (engine space is right-handed, the source is left-handed). `orientation` is copied
//! raw (`orientation_space: "source"`). Soundbank CRCs are zlib CRC-32 of the lower-case file name
//! ([`bank_crc`], stored as signed i32 in the XML).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::tuning::El;

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// zlib CRC-32 (IEEE) of the lower-case file name: the soundbank/event file id used by tiles and the lookup XML.
pub fn bank_crc(file_name: &str) -> u32 {
    let mut c = !0u32;
    for b in file_name.to_ascii_lowercase().bytes() {
        c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Event files the tiles' `eventfilecrc` can name (the 13 world `.fev`).
const WORLD_FEVS: &[&str] = &[
    "AMB_Default.fev",
    "AMB_Festival.fev",
    "AMB_Festival_Ambience.fev",
    "AMB_Foothills.fev",
    "AMB_Main_Town.fev",
    "AMB_Mountains.fev",
    "AMB_Plains.fev",
    "AMB_Quads.fev",
    "AMB_Red_Rock.fev",
    "AMB_Redstone.fev",
    "AMB_Reservoir.fev",
    "Colorado_Festival.fev",
    "Triggerable_Events.fev",
];

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub tiles: usize,
    pub track3d: usize,
    pub track2d: usize,
    pub trees: usize,
    pub crowds: usize,
    pub ambience_zones: usize,
    pub ambience_triangles: usize,
    pub reverb_zones: usize,
    pub reverb_triangles: usize,
    /// Lookup entries whose `crc` != `bank_crc(filename)`.
    pub crc_mismatches: usize,
    /// Tiles whose `TrackAudio soundbank` doesn't match the lookup-resolved bank.
    pub tile_bank_mismatches: usize,
    /// Distinct non-zero `eventfilecrc` values / how many resolve to a known world `.fev`.
    pub fev_crcs: usize,
    pub fev_crcs_resolved: usize,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} tiles, {} track3d, {} track2d, {} trees, {} crowds, {} ambience zones ({} triangles), \
             {} reverb zones ({} triangles), {} crc mismatches, {} tile/bank mismatches, {}/{} event crcs resolved",
            self.tiles,
            self.track3d,
            self.track2d,
            self.trees,
            self.crowds,
            self.ambience_zones,
            self.ambience_triangles,
            self.reverb_zones,
            self.reverb_triangles,
            self.crc_mismatches,
            self.tile_bank_mismatches,
            self.fev_crcs_resolved,
            self.fev_crcs,
        )
    }
}

// ---------------------------------------------------------------- XML helpers

fn kid<'a>(e: &'a El, name: &str) -> Option<&'a El> {
    e.kids.iter().find(|k| k.name == name)
}

fn req<'a>(e: &'a El, name: &str) -> Result<&'a El> {
    kid(e, name).with_context(|| format!("<{}> has no <{name}>", e.name))
}

/// Recursive search in document order, `e` itself included (`ElementTree.iter`).
fn descendants<'a>(e: &'a El, name: &str, out: &mut Vec<&'a El>) {
    if e.name == name {
        out.push(e);
    }
    for k in &e.kids {
        descendants(k, name, out);
    }
}

fn all<'a>(e: &'a El, name: &str) -> Vec<&'a El> {
    let mut v = Vec::new();
    descendants(e, name, &mut v);
    v
}

fn attr<'a>(e: &'a El, key: &str) -> Result<&'a str> {
    e.attr(key).with_context(|| format!("<{}> missing attribute {key}", e.name))
}

fn num(e: &El, key: &str) -> Result<f64> {
    let s = attr(e, key)?;
    s.trim().parse::<f64>().with_context(|| format!("<{}> {key}={s:?}", e.name))
}

fn int(e: &El, key: &str) -> Result<i64> {
    let s = attr(e, key)?;
    s.trim().parse::<i64>().with_context(|| format!("<{}> {key}={s:?}", e.name))
}

/// int → float → string (`"1"` is 1, `"1.0"` is 1.0; non-finite floats stay strings, JSON can't hold them).
fn autotype(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() {
            return json!(f);
        }
    }
    json!(s)
}

fn typed_attrs(e: &El) -> serde_json::Map<String, Value> {
    e.attrs.iter().map(|(k, v)| (k.clone(), autotype(v))).collect()
}

fn route_ids(s: &str) -> Vec<Value> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let digits = p.trim_start_matches('-');
            match p.parse::<i64>() {
                Ok(i) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => json!(i),
                _ => json!(p),
            }
        })
        .collect()
}

/// `(x, y, -z)`.
fn xyz(e: &El, prefix: &str) -> Result<[f64; 3]> {
    Ok([num(e, &format!("{prefix}x"))?, num(e, &format!("{prefix}y"))?, -num(e, &format!("{prefix}z"))?])
}

fn pos_json(p: [f64; 3]) -> Value {
    json!({"x": p[0], "y": p[1], "z": p[2]})
}

fn dump(v: &Value, path: &Path) -> Result<()> {
    std::fs::write(path, serde_json::to_vec(v)?).with_context(|| path.display().to_string())
}

// ---------------------------------------------------------------- templates

struct Templates {
    amb: Vec<Value>,
    rev: Vec<Value>,
    banks: Vec<Value>,
    bank_by_crc: HashMap<i64, String>,
    amb_name: HashMap<i64, String>,
    rev_name: HashMap<i64, String>,
    crc_mismatches: usize,
}

fn template_list(root: &El) -> (Vec<Value>, Vec<String>) {
    let mut docs = Vec::new();
    let mut names = Vec::new();
    for (i, t) in all(root, "Template").into_iter().enumerate() {
        let mut m = serde_json::Map::new();
        m.insert("index".into(), json!(i));
        m.extend(typed_attrs(t));
        docs.push(Value::Object(m));
        names.push(t.attr("Name").unwrap_or("").to_owned());
    }
    (docs, names)
}

fn load_templates(amb_xml: &str, rev_xml: &str, lookup_xml: &str) -> Result<Templates> {
    let (amb, amb_names) = template_list(&El::parse(amb_xml).context("colorado_ambience.xml")?);
    let (rev, rev_names) = template_list(&El::parse(rev_xml).context("colorado_reverb.xml")?);
    let lk = El::parse(lookup_xml).context("colorado_soundbank_lookup.xml")?;
    let (mut banks, mut bank_by_crc, mut crc_mismatches) = (Vec::new(), HashMap::new(), 0);
    for b in all(&lk, "Soundbank") {
        let crc = int(b, "crc")?;
        let filename = attr(b, "filename")?.to_owned();
        let crc_u32 = (crc & 0xFFFF_FFFF) as u32;
        if bank_crc(&filename) != crc_u32 {
            crc_mismatches += 1;
            eprintln!("  soundscape: lookup crc mismatch for {filename}: {crc_u32} != {}", bank_crc(&filename));
        }
        banks.push(json!({"crc": crc, "crc_u32": crc_u32, "filename": filename}));
        bank_by_crc.insert(crc, filename);
    }
    let prefix = |n: &str| n.split('_').next().and_then(|p| p.parse::<i64>().ok());
    let amb_name = amb_names.iter().filter_map(|n| Some((prefix(n)?, n.clone()))).collect();
    let rev_name = rev_names.iter().filter_map(|n| Some((prefix(n)?, n.clone()))).collect();
    Ok(Templates { amb, rev, banks, bank_by_crc, amb_name, rev_name, crc_mismatches })
}

// ---------------------------------------------------------------- tiles

#[derive(Default)]
struct Counts {
    track3d: usize,
    track2d: usize,
    trees: usize,
    crowds: usize,
    amb_zones: usize,
    amb_tris: usize,
    rev_zones: usize,
    rev_tris: usize,
}

impl Counts {
    fn json(&self) -> Value {
        json!({
            "track3d_objects": self.track3d, "track2d_objects": self.track2d,
            "trees": self.trees, "crowds": self.crowds,
            "ambience_zones": self.amb_zones, "ambience_triangles": self.amb_tris,
            "reverb_zones": self.rev_zones, "reverb_triangles": self.rev_tris,
        })
    }
}

fn obj_list(parent: &El, pts: &mut Vec<[f64; 3]>) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for o in parent.kids.iter().filter(|k| k.name == "Object") {
        let ori = req(o, "Orientation")?;
        let (p1, p2) = (xyz(req(o, "Position1")?, "")?, xyz(req(o, "Position2")?, "")?);
        pts.push(p1);
        pts.push(p2);
        out.push(json!({
            "object_name": o.attr("objectName"),
            "event_path": o.attr("eventPath"),
            "event_name": o.attr("eventName"),
            "route_ids": route_ids(o.attr("routeID").unwrap_or("")),
            "position1": pos_json(p1),
            "position2": pos_json(p2),
            "orientation": {"i": num(ori, "i")?, "j": num(ori, "j")?, "k": num(ori, "k")?,
                            "rotateX": num(ori, "rotateX")?, "rotateY": num(ori, "rotateY")?},
        }));
    }
    Ok(out)
}

fn shared(e: &El) -> Result<Value> {
    Ok(json!({
        "soundbank": e.attr("soundbank").unwrap_or(""),
        "shared3d_count": req(e, "Shared3D")?.kids.len(),
        "shared2d_count": req(e, "Shared2D")?.kids.len(),
    }))
}

fn convert_tile(tile: &str, xml: &str, t: &Templates, pts_ev: &mut BTreeSet<i64>) -> Result<(Value, Value, Counts, bool)> {
    let root = El::parse(xml)?;
    let ta = req(&root, "TrackAudio")?;
    let sb_crc = int(&root, "soundbankfilecrc")?;
    let ev_crc = int(&root, "eventfilecrc")?;
    if ev_crc != 0 {
        pts_ev.insert(ev_crc);
    }
    let mut pts: Vec<[f64; 3]> = Vec::new();
    let mut zone_xz: Vec<(f64, f64)> = Vec::new();

    let track3d = obj_list(req(ta, "Track3D")?, &mut pts)?;
    let track2d = obj_list(req(ta, "Track2D")?, &mut pts)?;

    let mut spots = |tag: &str| -> Result<Vec<Value>> {
        let mut v = Vec::new();
        for s in all(&root, tag) {
            let p = xyz(s, "position.")?;
            pts.push(p);
            v.push(json!({"route_ids": route_ids(s.attr("routeID").unwrap_or("")), "x": p[0], "y": p[1], "z": p[2],
                          "w": num(s, "position.w")?}));
        }
        Ok(v)
    };
    let trees = spots("Tree")?;
    let crowds = spots("Crowd")?;

    let mut amb_zones = Vec::new();
    let mut amb_tris = 0;
    for z in all(&root, "AmbienceZone") {
        let mut tris = Vec::new();
        for tri in z.kids.iter().filter(|k| k.name == "AmbienceTriangle") {
            let mut pp = Vec::new();
            for p in tri.kids.iter().filter(|k| k.name == "AmbienceTrianglePoint") {
                let (x, zz) = (num(p, "positionX")?, -num(p, "positionZ")?);
                zone_xz.push((x, zz));
                pp.push(json!({"x": x, "z": zz}));
            }
            tris.push(pp);
        }
        amb_tris += tris.len();
        let tid = int(z, "Template")?;
        amb_zones.push(json!({"template": tid, "template_name": t.amb_name.get(&tid),
            "min_height": num(z, "MinHeight")?, "max_height": num(z, "MaxHeight")?, "triangles": tris}));
    }

    let mut rev_zones = Vec::new();
    let mut rev_tris = 0;
    for z in all(&root, "ReverbZone") {
        let mut tris = Vec::new();
        for tri in z.kids.iter().filter(|k| k.name == "ReverbTriangle") {
            let mut pp = Vec::new();
            for p in tri.kids.iter().filter(|k| k.name == "ReverbTrianglePoint") {
                let (x, zz) = (num(p, "positionX")?, -num(p, "positionZ")?);
                zone_xz.push((x, zz));
                pp.push(json!({"x": x, "z": zz, "value": int(p, "value")?}));
            }
            tris.push(pp);
        }
        rev_tris += tris.len();
        let (t0, t1) = (int(z, "Template0")?, int(z, "Template1")?);
        rev_zones.push(json!({"template0": t0, "template0_name": t.rev_name.get(&t0),
            "template1": t1, "template1_name": t.rev_name.get(&t1),
            "min_height": num(z, "MinHeight")?, "max_height": num(z, "MaxHeight")?, "triangles": tris}));
    }

    let bank_file = t.bank_by_crc.get(&sb_crc);
    let sb = ta.attr("soundbank").unwrap_or("");
    let mismatch = !sb.is_empty() && bank_file.map(String::as_str) != Some(format!("{sb}.fsb").as_str());

    let bbox = if pts.is_empty() && zone_xz.is_empty() {
        Value::Null
    } else {
        let fold = |it: Vec<f64>, lo: bool| {
            it.into_iter().fold(if lo { f64::INFINITY } else { f64::NEG_INFINITY }, |a, b| if lo { a.min(b) } else { a.max(b) })
        };
        let xs: Vec<f64> = pts.iter().map(|p| p[0]).chain(zone_xz.iter().map(|p| p.0)).collect();
        let zs: Vec<f64> = pts.iter().map(|p| p[2]).chain(zone_xz.iter().map(|p| p.1)).collect();
        let ys: Vec<f64> = pts.iter().map(|p| p[1]).collect();
        let y = |lo| if ys.is_empty() { Value::Null } else { json!(fold(ys.clone(), lo)) };
        json!({"min": [fold(xs.clone(), true), y(true), fold(zs.clone(), true)],
               "max": [fold(xs, false), y(false), fold(zs, false)]})
    };

    let counts = Counts {
        track3d: track3d.len(),
        track2d: track2d.len(),
        trees: trees.len(),
        crowds: crowds.len(),
        amb_zones: amb_zones.len(),
        amb_tris,
        rev_zones: rev_zones.len(),
        rev_tris,
    };
    let doc = json!({
        "tile": tile,
        "source": format!("media/tracks/colorado/bin.zip:{tile}.soundscape"),
        "event_file_crc": ev_crc,
        "soundbank_file_crc": sb_crc,
        "soundbank_file": bank_file,
        "track_audio": {
            "soundbank": sb,
            "track3d": track3d,
            "track2d": track2d,
            "shared": shared(req(ta, "SharedAudio")?)?,
            "shared_dynamic": shared(req(ta, "SharedAudioDynamic")?)?,
        },
        "trees": trees,
        "crowds": crowds,
        "ambience_zones": amb_zones,
        "reverb_zones": rev_zones,
    });
    Ok((doc, bbox, counts, mismatch))
}

/// Converts every tile (`(file stem, xml text)`, sorted) and the three template XMLs into `out_dir`.
pub fn build(tiles: &[(String, String)], ambience_xml: &str, reverb_xml: &str, lookup_xml: &str, out_dir: &Path) -> Result<Stats> {
    std::fs::create_dir_all(out_dir.join("_templates"))?;
    let t = load_templates(ambience_xml, reverb_xml, lookup_xml)?;
    let base = out_dir.join("_templates");
    dump(&json!({"source": "media/tracks/colorado/colorado_ambience.xml", "count": t.amb.len(), "templates": t.amb}), &base.join("ambience.json"))?;
    dump(&json!({"source": "media/tracks/colorado/colorado_reverb.xml", "count": t.rev.len(), "templates": t.rev}), &base.join("reverb.json"))?;
    dump(
        &json!({"source": "media/tracks/colorado/colorado_soundbank_lookup.xml", "count": t.banks.len(), "soundbanks": t.banks}),
        &base.join("soundbank_lookup.json"),
    )?;

    let mut stats = Stats { crc_mismatches: t.crc_mismatches, ..Default::default() };
    let (mut index, mut totals, mut ev_crcs) = (Vec::new(), Counts::default(), BTreeSet::new());
    for (tile, xml) in tiles {
        let (doc, bbox, c, mismatch) = convert_tile(tile, xml, &t, &mut ev_crcs).with_context(|| tile.clone())?;
        dump(&doc, &out_dir.join(format!("{tile}.json")))?;
        stats.tile_bank_mismatches += mismatch as usize;
        index.push(json!({
            "tile": tile,
            "file": format!("soundscape/{tile}.json"),
            "soundbank_file": doc["soundbank_file"],
            "bbox": bbox,
            "counts": c.json(),
        }));
        totals.track3d += c.track3d;
        totals.track2d += c.track2d;
        totals.trees += c.trees;
        totals.crowds += c.crowds;
        totals.amb_zones += c.amb_zones;
        totals.amb_tris += c.amb_tris;
        totals.rev_zones += c.rev_zones;
        totals.rev_tris += c.rev_tris;
    }
    dump(
        &json!({
            "source": "media/tracks/colorado/bin.zip",
            "tile_count": index.len(),
            "totals": totals.json(),
            "coordinate_space": "engine (Z negated at setup; source is left-handed)",
            "orientation_space": "source",
            "tiles": index,
        }),
        &out_dir.join("index.json"),
    )?;

    let known: BTreeMap<u32, &str> = WORLD_FEVS.iter().map(|f| (bank_crc(f), *f)).collect();
    stats.fev_crcs = ev_crcs.len();
    stats.fev_crcs_resolved = ev_crcs.iter().filter(|c| known.contains_key(&((**c & 0xFFFF_FFFF) as u32))).count();
    stats.tiles = tiles.len();
    stats.track3d = totals.track3d;
    stats.track2d = totals.track2d;
    stats.trees = totals.trees;
    stats.crowds = totals.crowds;
    stats.ambience_zones = totals.amb_zones;
    stats.ambience_triangles = totals.amb_tris;
    stats.reverb_zones = totals.rev_zones;
    stats.reverb_triangles = totals.rev_tris;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_lookup() {
        assert_eq!(bank_crc("amb_default.fsb"), 3047655695);
        assert_eq!(bank_crc("AMB_Default.fsb"), 3047655695);
    }

    #[test]
    fn autotype_follows_python() {
        assert_eq!(autotype("1"), json!(1));
        assert_eq!(autotype("1.0"), json!(1.0));
        assert_eq!(autotype("0_TrackOpen"), json!("0_TrackOpen"));
        assert_eq!(autotype("nan"), json!("nan"));
    }

    #[test]
    fn route_ids_mixed() {
        assert_eq!(route_ids("1; -2;x;"), vec![json!(1), json!(-2), json!("x")]);
    }

    /// Needs the disc and reads the 2.9 GB `bin.zip`: `cargo test -p fh1-audio -- --ignored`.
    #[test]
    #[ignore]
    fn disc_totals() {
        let disc = std::env::var("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|_| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        if !track.join("bin.zip").exists() {
            return;
        }
        let mut ar = fh1_formats::zip::Archive::open(track.join("bin.zip")).unwrap();
        let mut seen = BTreeSet::new();
        let mut tiles = Vec::new();
        for e in ar.entries.clone() {
            let base = e.name.rsplit(['/', '\\']).next().unwrap().to_owned();
            let Some(stem) = base.strip_suffix(".soundscape") else { continue };
            if seen.insert(base.to_ascii_lowercase()) {
                tiles.push((stem.to_owned(), String::from_utf8(ar.read(&e).unwrap()).unwrap()));
            }
        }
        tiles.sort();
        let rd = |n: &str| std::fs::read_to_string(track.join(n)).unwrap();
        let out = std::env::temp_dir().join("fh1_soundscape_test");
        let s = build(&tiles, &rd("colorado_ambience.xml"), &rd("colorado_reverb.xml"), &rd("colorado_soundbank_lookup.xml"), &out).unwrap();
        assert_eq!(
            (s.tiles, s.track3d, s.track2d, s.trees, s.crowds),
            (1450, 33356, 0, 25845, 6400)
        );
        assert_eq!((s.ambience_zones, s.ambience_triangles, s.reverb_zones, s.reverb_triangles), (475, 3646, 377, 4076));
        assert_eq!(s.crc_mismatches, 0);
    }
}
