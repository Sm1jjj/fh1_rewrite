//! Checks against the user's own extracted disc (never committed): every UI scene, every string
//! table, every vector font. Set `FH1_DISC` to the extracted disc root (default: `<workspace>/disc`);
//! skipped if absent.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use fh1_formats::zip::Archive;
use fh1_ui::anark::{bgf::ObjectKind, fbf::Payload, Scene};
use fh1_ui::hash::{ahash, gamedb_ref, str_hash};
use fh1_ui::strtable::{StrTable, StringTables};
use fh1_ui::vfont::{Font, FontMap};

fn disc() -> Option<PathBuf> {
    let p = std::env::var_os("FH1_DISC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
    p.join("media/UI.zip").is_file().then_some(p)
}

const LANGS: [&str; 22] = [
    "EN", "GB", "DE", "FR", "ES", "MX", "IT", "nl", "br", "cz", "PL", "HU", "RU", "DA", "FI", "NB", "SV",
    "JP", "KO", "CHT", "LOC", "DEV",
];

/// stem → [bgf, fbf, bsg] bytes.
fn scene_files(root: &Path) -> BTreeMap<String, [Option<Vec<u8>>; 3]> {
    let mut ar = Archive::open(root.join("media/UI.zip")).unwrap();
    let mut files: BTreeMap<String, [Option<Vec<u8>>; 3]> = BTreeMap::new();
    for e in ar.entries.clone() {
        let name = e.name.replace('\\', "/");
        let Some(file) = name.strip_prefix("Scenes/ui4/") else { continue };
        let Some((stem, ext)) = file.rsplit_once('.') else { continue };
        let slot = match ext.to_lowercase().as_str() {
            "bgf" => 0,
            "fbf" => 1,
            "bsg" => 2,
            _ => continue,
        };
        files.entry(stem.to_string()).or_default()[slot] = Some(ar.read(&e).unwrap());
    }
    files
}

#[test]
fn all_scenes() {
    let Some(root) = disc() else {
        eprintln!("skipped: no extracted disc");
        return;
    };
    let files = scene_files(&root);
    let (mut n_bgf, mut n_triples) = (0, 0);
    let mut t = [0usize; 9]; // bytes nodes objects slides tracks keys actions meshes texrefs
    for (name, [bgf, fbf, bsg]) in &files {
        let bgf = bgf.as_ref().unwrap_or_else(|| panic!("{name}: no .bgf"));
        let s = Scene::load(bgf, fbf.as_deref(), bsg.as_deref()).unwrap_or_else(|e| panic!("{name}: {e}"));
        n_bgf += 1;
        let b = &s.bgf;
        t[0] += bgf.len();
        t[2] += b.objects.len();
        t[3] += b.slides.len();
        t[4] += b.tracks.len();
        t[5] += b.tracks.iter().map(|t| t.keys.len()).sum::<usize>();
        t[6] += b.actions().count();
        // structural checks from the spec
        for o in &b.objects {
            assert!(o.parent < b.objects.len() as i32, "{name}: parent index");
            if o.kind == ObjectKind::Component {
                let info = o.component.unwrap();
                let idx = b.objects.iter().position(|x| std::ptr::eq(x, o)).unwrap() as i32;
                let mut slides = b.slides_of(idx);
                let master = slides.next().unwrap();
                assert_eq!(master.name, "Master Slide", "{name}");
                assert_eq!(info.duration, master.end, "{name}");
                assert_eq!(info.n_slides as usize, 1 + slides.count(), "{name}");
            }
        }
        for sl in &b.slides {
            for r in sl.entries.iter().flat_map(|e| e.refs.iter()) {
                if let Some(ti) = r.track() {
                    assert!(ti < b.tracks.len(), "{name}: track ref");
                }
            }
        }
        if let (Some(fd), Some(gd), Some(f), Some(g)) = (fbf, bsg, &s.fbf, &s.bsg) {
            n_triples += 1;
            t[0] += fd.len() + gd.len();
            t[1] += g.nodes.len();
            t[7] += f.meshes.len();
            t[8] += f.images().count();
            assert_eq!(f.count as usize, f.records.len(), "{name}: fbf count");
            let mut mesh_refs = vec![0; f.meshes.len()];
            for r in &f.records {
                if let Payload::Model { mesh, .. } = r.payload {
                    mesh_refs[mesh as usize] += 1;
                }
            }
            assert!(mesh_refs.iter().all(|&n| n == 1), "{name}: each mesh used once");
            for (_, img) in f.images() {
                let m = f.record(img.material).unwrap();
                assert!(matches!(m.payload, Payload::Material(_)), "{name}: image -> material");
                let rebuilt = img.uv_matrix_from_params();
                for (a, b) in rebuilt.iter().zip(img.uv_matrix) {
                    assert!((a - b).abs() < 1e-3, "{name}: uv matrix {rebuilt:?} vs {:?}", img.uv_matrix);
                }
            }
            let res: usize = f
                .records
                .iter()
                .filter(|r| matches!(r.payload, Payload::Material(_) | Payload::Image(_)))
                .count();
            assert_eq!(g.resources.len(), res, "{name}: bsg resources");
            assert_eq!(g.nodes.len() + res, f.records.len(), "{name}: bsg nodes");
        }
    }
    eprintln!("{n_bgf} bgf, {n_triples} triples, totals {t:?}");
    assert_eq!((n_bgf, n_triples), (230, 205));
    assert_eq!(t, [48_710_522, 38_829, 85_509, 23_529, 81_251, 152_223, 104_739, 16_646, 14_808]);
}

/// S1 record names hash to the fbf names (not images: bgf hashes the original-case file name).
#[test]
fn object_name_hashes() {
    let Some(root) = disc() else { return };
    let files = scene_files(&root);
    let [Some(bgf), Some(fbf), Some(bsg)] = &files["925_PAUSE_MENU"] else { panic!("925 missing") };
    let s = Scene::load(bgf, Some(fbf), Some(bsg)).unwrap();
    let f = s.fbf.as_ref().unwrap();
    let names: HashMap<u32, &str> = f.records.iter().map(|r| (r.id, r.name.as_str())).collect();
    let (mut hits, mut total) = (0, 0);
    for o in &s.bgf.objects {
        let Some(n) = names.get(&o.id) else { continue };
        let ok = o.name_hash == ahash(n.as_bytes());
        match o.kind {
            // Exact for these kinds (the reference reader agrees on this scene).
            ObjectKind::Material | ObjectKind::Component | ObjectKind::Camera | ObjectKind::Light => {
                assert!(ok, "{n} (id {})", o.id)
            }
            // Model nodes are often hashed under another name (e.g. BG_Noise); images use the
            // original-case file name. Only count.
            ObjectKind::Node | ObjectKind::Text => {
                hits += usize::from(ok);
                total += 1;
            }
            _ => {}
        }
    }
    eprintln!("925_PAUSE_MENU: {hits}/{total} node/text name hashes match");
    assert_eq!((hits, total), (126, 186));
}

#[test]
fn string_tables() {
    let Some(root) = disc() else { return };
    let (mut all_files, mut all_texts, mut all_names) = (0, 0, 0);
    for lang in LANGS {
        let mut ar = Archive::open(root.join(format!("media/stringtables/{lang}.zip"))).unwrap();
        let mut n_files = 0;
        for e in ar.entries.clone() {
            if !e.name.to_lowercase().ends_with(".str") {
                continue;
            }
            let t = StrTable::parse(&ar.read(&e).unwrap()).unwrap_or_else(|err| panic!("{lang} {}: {err}", e.name));
            for (h, n) in &t.names {
                assert_eq!(str_hash(&n.name), *h, "{lang} {} {}", e.name, n.name);
            }
            n_files += 1;
            all_texts += t.texts.len();
            all_names += t.names.len();
        }
        assert!((109..=113).contains(&n_files), "{lang}: {n_files}");
        all_files += n_files;
    }
    assert_eq!(all_files, 2_481);
    assert_eq!((all_texts, all_names), (223_625, 223_625));
    let en = StringTables::load_language(&root, "EN").unwrap();
    assert_eq!(en.resolve("Tips:IDS_PopularityTitle"), Some("POPULARITY"));
    assert_eq!(en.resolve("ingame:IDS_Paused"), Some("PAUSED"));
    // gamedb Data_Car.DisplayName of ALF_8C_08 (_&3444170713, computed from the hash formula).
    let n = gamedb_ref("Data_Car", "IDS_DisplayName_1032");
    assert_eq!(n, 3_444_170_713);
    assert_eq!(en.resolve("_&3444170713"), Some("8C Competizione"));
    assert_eq!(en.gamedb(3_444_160_236), Some("250 GTO"));
}

#[test]
fn fonts() {
    let Some(root) = disc() else { return };
    let mut ar = Archive::open(root.join("media/ui/Fonts.zip")).unwrap();
    let expected: HashMap<&str, usize> = [
        ("a", 237), ("b", 237), ("d", 237), ("e", 237), ("aru", 303), ("bru", 303), ("dru", 303), ("eru", 303),
        ("c", 79), ("boing", 81), ("dg1", 19), ("dg2", 19), ("dg3", 19), ("dg4", 19), ("dg5", 19), ("sym", 6),
        // comparison/ (FontCompiler before/after builds of A)
        ("a_before", 225), ("a_after", 409),
    ]
    .into();
    let mut seen = 0;
    let mut map = None;
    for e in ar.entries.clone() {
        let base = e.name.rsplit('/').next().unwrap_or(&e.name).to_lowercase();
        if base == "fontmap.xml" {
            map = Some(FontMap::parse(&String::from_utf8_lossy(&ar.read(&e).unwrap())));
        }
        let Some(stem) = base.strip_suffix(".dt") else { continue };
        let stem = stem.replacen("_vector_aa", "", 1);
        let stem = stem.as_str();
        let f = Font::parse(&ar.read(&e).unwrap()).unwrap_or_else(|err| panic!("{base}: {err}"));
        assert_eq!(f.glyphs.len(), expected[stem], "{base}");
        let tris: usize = f.glyphs.values().map(|g| (g.inner.indices.len() + g.outer.indices.len()) / 3).sum();
        assert_eq!(tris * 3, f.num_indices as usize, "{base}: every triangle in one mesh");
        for g in f.glyphs.values() {
            assert!(g.inner.verts.iter().all(|v| v[0] >= 0.0), "{base}: inner x >= 0");
            assert!(g.outer.indices.chunks(3).all(|t| t.iter().any(|&i| g.outer.verts[i as usize][0] < 0.0)));
            assert_eq!(g.hash_slot(&f), Some(g.codepoint), "{base}: direct hash slot");
        }
        if ["a", "b", "d", "e"].contains(&stem) {
            let m = f.metrics;
            assert_eq!((m.win_ascent, m.win_descent, m.em), (1115, 372, 991), "{base}");
            assert_eq!(f.default_char, 9633);
        }
        seen += 1;
    }
    assert_eq!(seen, expected.len());
    let map = map.expect("fontmap.xml");
    assert_eq!(map.resolve("horizon_e"), Some("E"));
    assert_eq!(map.resolve("Horizon_C"), Some("A"));
    assert_eq!(map.resolve("horizon_dg2"), Some("DG2"));
}

trait HashSlot {
    fn hash_slot(&self, f: &Font) -> Option<u32>;
}

impl HashSlot for fh1_ui::vfont::Glyph {
    /// Codepoint stored in the glyph's direct slot (`codepoint % hash_size`).
    fn hash_slot(&self, f: &Font) -> Option<u32> {
        let n = f.hash_table.len() as u32;
        f.hash_table.get((self.codepoint % n) as usize).map(|&(u, _)| u as u32)
    }
}

/// Player behaviour on the real HUD: element-scoped events and the radio widget's slide chain.
#[test]
fn hud_radio_events() {
    use fh1_ui::player::Player;
    let Some(root) = disc() else { return };
    let files = scene_files(&root);
    let [Some(bgf), Some(fbf), Some(bsg)] = &files["947_HUD"] else { panic!("947 missing") };
    let mut p = Player::new(Scene::load(bgf, Some(fbf), Some(bsg)).unwrap());
    let radio = p.find("RadioStation").unwrap();
    let element = p.find("CompleteElement").unwrap();
    let slide = |p: &Player, c: usize| p.scene.bgf.slides[p.clock(c).unwrap().slide].name.clone();
    // SHOWN is handled per widget: on the radio element it runs nothing, globally it runs many.
    assert_eq!(p.fire_at("SHOWN", radio), 0);
    assert!(p.fire("SHOWN") > 0);
    // The radio widget: SHOW_STATION -> CHANGESTATION (plays, 3 s), HIDE_INFO -> OFF.
    assert!(p.fire_at("SHOW_STATION", radio) > 0);
    assert_eq!(slide(&p, element), "CHANGESTATION");
    assert!(p.clock(element).unwrap().playing);
    p.update(3100.0);
    assert!(!p.clock(element).unwrap().playing);
    p.fire_at("HIDE_INFO", radio);
    assert_eq!(slide(&p, element), "OFF");
    p.fire_at("SHOW_INFO", radio);
    assert_eq!(slide(&p, element), "CHANGETRACK");
    // Contract paths resolve from the behaviour record.
    assert!(p.resolve_path(radio, "CompleteElement.Content.Track.Text").is_some());
}

/// colorado.nav: counts from the format notes, and the drawable roads.
#[test]
fn nav_roads() {
    let Some(root) = disc() else { return };
    let d = std::fs::read(root.join("media/tracks/colorado/colorado.nav")).unwrap();
    let nav = fh1_ui::nav::Nav::parse(&d).unwrap();
    assert_eq!((nav.nodes.len(), nav.ways.len()), (12036, 459));
    let roads = nav.roads();
    let count = |t: &str| roads.iter().filter(|(k, _)| k == t).count();
    assert_eq!((count("b"), count("a"), count("freeway"), count("dirt")), (202, 100, 48, 35));
    // Engine space: collision Z negated; the road bounds are x -6269..6816, z -6124..3802 (collision).
    let (zmin, zmax) = roads.iter().flat_map(|(_, p)| p.iter()).fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p[1]), b.max(p[1])));
    assert!(zmin > -3810.0 && zmax < 6130.0, "{zmin} {zmax}");
}
