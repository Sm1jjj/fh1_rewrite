//! Checks against the user's own extracted disc (never committed): archives, models, textures.
//! Set `FH1_DISC` to the extracted disc root (default: `<workspace>/disc`); skipped if absent.

use std::path::PathBuf;

use fh1_formats::zip::Archive;

fn disc() -> Option<PathBuf> {
    let p = std::env::var_os("FH1_DISC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
    p.join("default.xex").is_file().then_some(p)
}

fn verify_all(rel: &str) {
    let Some(root) = disc() else {
        eprintln!("skipped: no extracted disc");
        return;
    };
    let mut ar = Archive::open(root.join(rel)).unwrap();
    let entries = ar.entries.clone();
    assert!(!entries.is_empty());
    for e in &entries {
        ar.read(e).unwrap_or_else(|err| panic!("{rel}: {err}"));
    }
}

#[test]
fn car_archive() {
    verify_all("media/cars/ALF_8C_08.zip");
}

/// Contains LZX uncompressed blocks of odd length spanning frames (the lzxd padding patch).
#[test]
fn uncompressed_lzx_blocks() {
    verify_all("media/UI.zip");
    verify_all("media/cars/Driver.zip");
}

/// Method 8 entries.
#[test]
fn deflate_archive() {
    verify_all("media/audio/cars/Engines/DSPTuning.zip");
}

#[test]
fn headerless_world_archive_directory() {
    let Some(root) = disc() else { return };
    let ar = Archive::open(root.join("media/tracks/colorado/bin.zip")).unwrap();
    // 16-bit count in the end record wraps (33449); the real directory is larger.
    assert_eq!(ar.entries.len(), 230_057);
}

fn car_entry(root: &std::path::Path, car: &str, name: &str) -> Vec<u8> {
    let mut ar = Archive::open(root.join(format!("media/cars/{car}.zip"))).unwrap();
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned().unwrap();
    ar.read(&e).unwrap()
}

/// Main carbin: 33 sections, and the decoded mesh spans gamedb's PristineBoundingBox for this
/// car (X/Y equal, Z mirrored: gamedb's front is +Z, the mesh's is -Z).
#[test]
fn carbin_alfa_main_matches_gamedb_bounds() {
    let Some(root) = disc() else { return };
    let c = fh1_formats::carbin::parse(&car_entry(&root, "ALF_8C_08", "ALF_8C_08.carbin")).unwrap();
    assert_eq!(c.sections.len(), 33);
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for s in &c.sections {
        for sub in s.subsections.iter().filter(|x| x.lod == 1) {
            for &i in &sub.indices {
                let p = s.vertices_for(sub)[i as usize].position;
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k] + s.offset[k]);
                    hi[k] = hi[k].max(p[k] + s.offset[k]);
                }
            }
        }
    }
    // Data_CarBody 1032000: X ±1.066934, Y -0.353248..1.218904, Z -2.209913..2.381343
    let close = |a: f32, b: f32| (a - b).abs() < 0.002;
    assert!(close(lo[0], -1.066934) && close(hi[0], 1.066934), "x {lo:?} {hi:?}");
    assert!(close(lo[1], -0.353248) && close(hi[1], 1.218904), "y {lo:?} {hi:?}");
    assert!(close(lo[2], -2.381343) && close(hi[2], 2.209913), "z {lo:?} {hi:?}");
}

#[test]
fn carbin_lod0_and_brakes_parse() {
    let Some(root) = disc() else { return };
    let lod0 = fh1_formats::carbin::parse(&car_entry(&root, "ALF_8C_08", "ALF_8C_08_lod0.carbin")).unwrap();
    assert_eq!(lod0.sections.len(), 20);
    let rotor = fh1_formats::carbin::parse(&car_entry(&root, "ALF_8C_08", "ALF_8C_08_rotorLF_LOD0.carbin")).unwrap();
    assert!(!rotor.sections.is_empty());
}

#[test]
fn xds_body_atlas_decodes() {
    let Some(root) = disc() else { return };
    let (h, img) = fh1_formats::xds::decode_base(&car_entry(&root, "ALF_8C_08", "nodamage_LOD0.xds")).unwrap();
    assert_eq!((h.width, h.height, h.format), (2048, 2048, fh1_formats::xds::Format::Dxt5));
    assert_eq!(h.max_mip, 11);
    let rgba = fh1_formats::xds::to_rgba8(&img).unwrap();
    // The paint region (atlas centre) is white so the car colour shows through.
    let px = |x: usize, y: usize| &rgba[(y * 2048 + x) * 4..][..3];
    assert!(px(1300, 1500).iter().all(|&c| c > 240), "{:?}", px(1300, 1500));
}


/// Colorado visual meshes: every unique `.rmb.bin` parses; totals match the recon (17,596,869 tris).
#[test]
fn colorado_track_models() {
    let Some(root) = disc() else { return };
    let mut ar = Archive::open(root.join("media/tracks/colorado/bin.zip")).unwrap();
    let mut seen = std::collections::HashSet::new();
    let (mut files, mut tris, mut lod0_tris) = (0usize, 0usize, 0usize);
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !seen.insert(n) {
            continue;
        }
        let m = fh1_formats::rmb::parse(&ar.read(&e).unwrap()).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        files += 1;
        for s in &m.submodels {
            let t: usize = s.meshes.iter().map(|x| x.indices.len() / 3).sum();
            tris += t;
            if s.lod() == 0 && !s.is_helper() {
                lod0_tris += t;
            }
        }
    }
    eprintln!("{files} track models, {tris} triangles ({lod0_tris} full-detail)");
    assert_eq!(files, 14_291);
    assert_eq!(tris, 17_596_869);
}

/// Vertex layouts: each submodel's shader declaration (its first mesh's material -> shader ->
/// shaders/track/<name>.fxobj) has exactly the submodel's stride; normals decode to unit length.
#[test]
fn colorado_vertex_layouts() {
    let Some(root) = disc() else { return };
    let mut ar = Archive::open(root.join("media/tracks/colorado/bin.zip")).unwrap();
    let mut by_name = std::collections::HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase().replace(char::from(92), "/")).or_insert_with(|| e.clone());
    }
    let mut decls = std::collections::HashMap::new();
    let (mut subs, mut checked, mut normal_len, mut normals) = (0usize, 0usize, 0f64, 0usize);
    let mut names: Vec<String> = by_name.keys().filter(|n| n.ends_with(".rmb.bin")).cloned().collect();
    names.sort();
    for n in &names {
        let m = fh1_formats::rmb::parse(&ar.read(&by_name[n]).unwrap()).unwrap();
        for s in &m.submodels {
            subs += 1;
            let Some(shader) = m.submodel_shader(s) else { continue };
            let file = format!("shaders/track/{}obj", shader.rsplit([char::from(92), '/']).next().unwrap().to_ascii_lowercase());
            let decl = decls
                .entry(file.clone())
                .or_insert_with(|| fh1_formats::fxobj::vertex_decl(&ar.read(&by_name[&file]).unwrap()).unwrap())
                .clone();
            assert_eq!(decl.stride(), s.stride, "{n} {}: {file}", s.name);
            checked += 1;
            for v in s.attributes(&decl).normals.iter().take(50) {
                normal_len += ((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) as f64).sqrt();
                normals += 1;
            }
        }
    }
    let mean = normal_len / normals as f64;
    eprintln!("{checked}/{subs} submodels match their shader's stride; {} shaders; mean normal length {mean:.3}", decls.len());
    assert!(checked + 1 >= subs);
    assert!((mean - 1.0).abs() < 0.02);
}

/// Colorado textures: every .bix/_B.bix pair decodes its base level.
#[test]
fn colorado_textures() {
    let Some(root) = disc() else { return };
    let mut ar = Archive::open(root.join("media/tracks/colorado/bin.zip")).unwrap();
    let mut by_name = std::collections::HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase()).or_insert_with(|| e.clone());
    }
    let mut ids: Vec<String> = by_name.keys().filter(|n| n.ends_with(".bix") && !n.ends_with("_b.bix")).cloned().collect();
    ids.sort();
    let (mut ok, mut formats) = (0, std::collections::BTreeMap::new());
    for n in &ids {
        let base_name = n.trim_end_matches(".bix").to_string() + "_b.bix";
        let bix = ar.read(&by_name[n]).unwrap();
        let base = ar.read(&by_name[&base_name]).unwrap();
        let (h, img) = fh1_formats::bix::decode_base(&bix, &base).unwrap_or_else(|e| panic!("{n}: {e}"));
        fh1_formats::xds::to_rgba8(&img).unwrap_or_else(|e| panic!("{n}: {e}"));
        *formats.entry(format!("{:?}", h.format)).or_insert(0) += 1;
        ok += 1;
    }
    eprintln!("{ok} textures: {formats:?}");
    assert_eq!(ok, 5_234);
}
