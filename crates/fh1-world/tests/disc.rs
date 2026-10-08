//! Checks against the user's own extracted disc (never committed).
//! Set `FH1_DISC` to the extracted disc root (default: `<workspace>/disc`); skipped if absent.

use std::path::PathBuf;
use std::sync::OnceLock;

use fh1_world::World;

fn disc() -> Option<PathBuf> {
    let p = std::env::var_os("FH1_DISC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
    p.join("default.xex").is_file().then_some(p)
}

/// Loading all of Colorado takes a few seconds, so the tests share one copy.
fn colorado() -> Option<&'static World> {
    static W: OnceLock<Option<World>> = OnceLock::new();
    W.get_or_init(|| {
        let root = disc()?;
        let (w, s) = World::from_disc(&root, "colorado").unwrap();
        eprintln!("{s:?}: {} verts, {} tris", w.verts.len(), w.tris.len());
        Some(w)
    })
    .as_ref()
}

macro_rules! world_or_skip {
    () => {
        match colorado() {
            Some(w) => w,
            None => {
                eprintln!("skipped: no extracted disc");
                return;
            }
        }
    };
}

#[test]
fn colorado_loads_whole_map() {
    let root = match disc() {
        Some(r) => r,
        None => return eprintln!("skipped: no extracted disc"),
    };
    let (w, s) = World::from_disc(&root, "colorado").unwrap();
    // Totals before welding, as counted by the recon probe.
    assert_eq!(s.squares, 3201);
    assert_eq!(s.raw_verts, 1_729_941);
    assert_eq!(s.raw_tris, 2_007_595);
    assert_eq!(w.tris.len() + s.duplicate_tris + s.degenerate_tris, s.raw_tris);
    let (lo, hi) = w.bounds();
    // The .col grid spans x -6409..6833, z -6133..3815; meshes add an 8 m cushion at most.
    assert!(lo[0] > -6420.0 && hi[0] < 6845.0 && lo[2] > -6145.0 && hi[2] < 3830.0, "{lo:?} {hi:?}");
}

#[test]
fn surface_names_and_properties() {
    let w = world_or_skip!();
    let names: Vec<&str> = w.surfaces.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.len(), 68);
    assert_eq!(&names[..4], ["Asphalt", "Dirt", "Grass", "GuardRail"]);
    let asphalt = &w.surfaces[0];
    assert_eq!(asphalt.category.as_deref(), Some("HardWorld"));
    assert_eq!(asphalt.friction(), 1.0);
    assert_eq!(asphalt.offroadness(), 0.0);
    assert_eq!(asphalt.debug_color(), [80, 80, 80]);
    let dirt = &w.surfaces[1];
    assert_eq!(dirt.friction(), 0.95);
    assert_eq!(dirt.offroadness(), 1.0);
    // Every triangle's surface has a name.
    assert!(w.tris.iter().all(|t| (t.surface as usize) < w.surfaces.len()));
}

/// The surface index mapping, proven by geometry: walls are vertical, ground is flat.
#[test]
fn walls_are_vertical_and_ground_is_flat() {
    let w = world_or_skip!();
    let share_vertical = |id: u8| {
        let (mut n, mut vert) = (0usize, 0usize);
        for (i, t) in w.tris.iter().enumerate() {
            if t.surface == id {
                let [a, b, c] = w.tri_points(i as u32);
                let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                let nrm = [e1[1] * e2[2] - e1[2] * e2[1], e1[2] * e2[0] - e1[0] * e2[2], e1[0] * e2[1] - e1[1] * e2[0]];
                let len = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt();
                n += 1;
                vert += usize::from(nrm[1].abs() / len < 0.3);
            }
        }
        vert as f32 / n as f32
    };
    assert!(share_vertical(3) > 0.95, "GuardRail");
    assert!(share_vertical(14) > 0.95, "WireFence");
    assert!(share_vertical(1) < 0.05, "Dirt");
    assert!(share_vertical(2) < 0.05, "Grass");
    assert!(share_vertical(23) < 0.05, "Gravel");
}

#[test]
fn height_queries_hit_the_road() {
    let w = world_or_skip!();
    // Near the first vertex of 848.fiz (-4404.6, 40.1, -2758.6), a dirt-road square seen in the
    // recon hex dump. Exactly on a vertex a ray can slip between triangles, so step off it.
    let (x, z) = (-4404.0, -2758.0);
    let h = w.height_at(x, z).expect("ground near a known vertex");
    assert!((h.point[1] - 40.1).abs() < 1.5, "{h:?}");
    assert!(h.normal[1] > 0.9, "ground faces up: {h:?}");
    // A short suspension-style ray from 1 m above.
    let r = w.raycast([x, h.point[1] + 1.0, z], [0.0, -1.0, 0.0], 2.0).unwrap();
    assert!((r.t - 1.0).abs() < 0.01 && r.tri == h.tri, "{r:?}");
    // Every up-facing ground triangle is found from above at its centroid (sample of 2000).
    // (A third of Asphalt triangles are vertical kerb faces, which a vertical ray can't hit.)
    let faces_up = |ti: u32| {
        let [a, b, c] = w.tri_points(ti);
        let (e1, e2) = ([b[0] - a[0], b[1] - a[1], b[2] - a[2]], [c[0] - a[0], c[1] - a[1], c[2] - a[2]]);
        let n = [e1[1] * e2[2] - e1[2] * e2[1], e1[2] * e2[0] - e1[0] * e2[2], e1[0] * e2[1] - e1[1] * e2[0]];
        n[1].abs() > 0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
    };
    let ground: Vec<u32> = (0..w.tris.len() as u32)
        .filter(|&i| matches!(w.tris[i as usize].surface, 0 | 1 | 2) && faces_up(i))
        .step_by(300)
        .take(2000)
        .collect();
    let mut found = 0;
    for &ti in &ground {
        let [a, b, c] = w.tri_points(ti);
        let m = [(a[0] + b[0] + c[0]) / 3.0, (a[1] + b[1] + c[1]) / 3.0, (a[2] + b[2] + c[2]) / 3.0];
        // Something at or above the centroid (bridges and tunnels can cover it).
        if let Some(hit) = w.raycast([m[0], m[1] + 0.5, m[2]], [0.0, -1.0, 0.0], 1.0) {
            found += usize::from((hit.point[1] - m[1]).abs() < 0.05);
        }
    }
    assert!(found as f32 > 0.99 * ground.len() as f32, "{found} / {}", ground.len());
    // Far outside the map there is nothing.
    assert!(w.height_at(20_000.0, 20_000.0).is_none());
}

#[test]
fn sphere_finds_guardrail() {
    let w = world_or_skip!();
    // Take a guard-rail triangle and push a sphere into it from its face.
    let ti = w.tris.iter().position(|t| t.surface == 3).unwrap() as u32;
    let [a, b, c] = w.tri_points(ti);
    let mid = [(a[0] + b[0] + c[0]) / 3.0, (a[1] + b[1] + c[1]) / 3.0, (a[2] + b[2] + c[2]) / 3.0];
    let mut out = Vec::new();
    w.sphere_contacts(mid, 0.5, &mut out);
    let hit = out.iter().find(|c| c.tri == ti).expect("contact with the rail triangle");
    assert!(hit.depth > 0.49 && w.surfaces[hit.surface as usize].name == "GuardRail");
}

#[test]
fn save_load_round_trip() {
    let w = world_or_skip!();
    let dir = std::env::temp_dir().join(format!("fh1-world-test-{}", std::process::id()));
    w.save(&dir).unwrap();
    let back = World::load(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(back.same_as(w));
    let a = w.height_at(-4404.0, -2758.0).unwrap();
    let b = back.height_at(-4404.0, -2758.0).unwrap();
    assert_eq!((a.tri, a.t), (b.tri, b.t));
}
