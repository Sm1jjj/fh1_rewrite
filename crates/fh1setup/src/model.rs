//! Car model export: `.carbin` + `.xds` from a car zip -> glTF 2.0 (`model.gltf` + `model.bin`
//! + PNG atlases) and `model.json` (wheel hubs etc. for the engine).
//!
//! Coordinates are kept as stored: +Y up, front of the car towards -Z, left side at -X
//! (the `seatL` section sits at -X), metres. Origin is the bottom-centre of the body
//! (gamedb `BottomCenterWheelbasePos`): the ground is at y = -ride height and the hubs at
//! y = tyre radius - ride height. Verified via the underbody scrape spheres in MAXData/physicsdefinition
//! (y ≈ 0.03..0.08) and the body section's bounds (y = 0..1.22 on ALF_8C_08).
//! Note: gamedb / MAXData use the opposite Z sign (their bounding box is this one mirrored).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use fh1_formats::carbin::{self, Section, Subsection};
use fh1_formats::xds;
use fh1_formats::zip::Archive;

/// Corner order used everywhere: LF, RF, LR, RR.
pub const CORNERS: [&str; 4] = ["LF", "RF", "LR", "RR"];

pub struct Paint {
    pub rgb: u32,
    pub metallic: bool,
    /// Combo_Colors.Sequence (picks `ColorShaderSettings<N>.xml`).
    pub sequence: u32,
}

/// Stock tyre and rim of one axle (gamedb `Data_Car` sizes), metres.
#[derive(Clone, Copy, Debug)]
pub struct Axle {
    pub tyre_radius: f32,
    pub tyre_width: f32,
    pub rim_radius: f32,
}

/// The stock wheel: `media/wheels/<rim>.zip` (`List_Wheels.MediaName` of `Data_Car.StockWheelID`)
/// and the front/rear sizes it is fitted to.
pub struct Wheels<'a> {
    pub rim_zip: Option<&'a Path>,
    pub rim_media: &'a str,
    pub axles: [Axle; 2],
    /// Wheelbase bottom-centre in mesh space (`Data_CarBody.BottomCenterWheelbasePos`, stored sign): arch
    /// centres sit at hub + this Z (79 cars, fit slope 0.97, median residual 6 mm; non-zero on 149/176 cars, up
    /// to 0.35 m). MAXData and the physics are centred on this point (MAXData's own BottomCenterWheelbasePos is
    /// its mirror), so hubs stay wheelbase-centred and the glTF root is translated by -centre (`mesh_offset` in
    /// model.json; anything else drawn in mesh space, e.g. cockpit.gltf, needs it too). Y: INFERRED, same rule.
    pub centre: [f32; 3],
}

/// The stock body-kit letter per kit stem (`bumperf`, `bumperr`, `hood`, `skirtl`, `skirtr`, `wing`): `a` + the stock
/// row's gamedb `Sequence` (List_UpgradeCarBody{FrontBumper,RearBumper,Hood,SideSkirt}, List_UpgradeRearWing).
/// Sequence 0 = `a` on every car but VW_IECorrado_95 (rear bumper 2 -> `bumperRc`, skirts 1 -> `skirtLb`; its rear
/// bumper rows 0..5 match its six letters a..f). The letter = a + Sequence rule is INFERRED from that.
#[derive(Default, Clone)]
pub struct Kit(pub Vec<(&'static str, char)>);

impl Kit {
    pub(crate) fn letter(&self, stem: &str) -> char {
        self.0.iter().find(|(s, _)| *s == stem).map_or('a', |(_, l)| *l)
    }
}

/// `hub_height`: front and rear hub height above the model origin (tyre radius - ride height).
#[allow(clippy::too_many_arguments)]
pub fn export(zip: &Path, media: &str, paint: &Paint, kit: &Kit, maxdata: &Value, hub_height: [f32; 2], wheels: &Wheels, shared: &[(&'static str, String)], out: &Path) -> Result<Value> {
    let mut ar = Archive::open(zip)?;
    let mut read = |name: &str| -> Result<Option<Vec<u8>>> {
        match ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned() {
            Some(e) => Ok(Some(ar.read(&e)?)),
            None => Ok(None),
        }
    };

    let main = carbin::parse(&read(&format!("{media}.carbin"))?.context("main carbin missing")?)
        .context("main carbin")?;
    let lod0 = read(&format!("{media}_lod0.carbin"))?.and_then(|d| carbin::parse(&d).ok());

    // Atlases. A subsection samples one of these (chosen by name, see `material_for`).
    let mut atlases = HashMap::new();
    // FOR_F350SuperDuty_08, TOY_CorollaDX_95 and VOL_XC70_12 ship no `_LOD0` atlases (and no LOD0 model): their
    // LOD1 meshes sample the plain `nodamage` / `lights` (the game's LOD1 textures), same UV layout.
    for (key, file) in [("exterior", "nodamage"), ("interior", "interior"), ("lights", "lights")] {
        let data = match read(&format!("{file}_LOD0.xds"))? {
            Some(d) => Some(d),
            None => read(&format!("{file}.xds"))?,
        };
        if let Some(d) = data {
            let (_, img) = xds::decode_base(&d)?;
            let rgba = xds::to_rgba8(&img)?;
            let png = format!("{key}.png");
            write_png(&out.join(&png), img.width, img.height, &rgba)?;
            atlases.insert(key, png);
        }
    }

    // Rim model and its texture (`wheel_LOD0.xds`). The rim PS (shaders_v16 rim_V2, PS 465) shades
    // tex.rgb * lerp(TintColor, 1, tex.a) * PaintScale: RGB is the whole rim, alpha only marks the decals the
    // tint must not touch. TintColor is white in Tire/Normal.xml, so the alpha drops out; PaintScale goes on
    // the material (RIM_PAINT_SCALE).
    let rim = wheels.rim_zip.and_then(|z| read_rim(z, wheels.rim_media).ok().flatten());
    if let Some((_, Some((w, h, rgba)))) = &rim {
        let baked: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        write_png(&out.join("wheel.png"), *w, *h, &baked)?;
        atlases.insert("wheel", "wheel.png".to_owned());
    }
    // Shared.zip textures already written next to the model (tyre, grilles, bumper frame...).
    for (key, png) in shared {
        atlases.insert(key, png.clone());
    }

    let misc = &maxdata["Misc"];
    let wheelbase = misc["Wheelbase"].as_f64().unwrap_or(2.6) as f32;
    let front_track = misc["FrontTrackOuter"].as_f64().unwrap_or(1.6) as f32;
    let rear_track = misc["RearTrackOuter"].as_f64().unwrap_or(1.6) as f32;

    let mut g = Gltf::default();
    let mut children = Vec::new();
    // Wheel parts: the rim section (+ the LOD its geometry came from) and the main carbin's wheel
    // section, which also carries the tyre (the lod0 wheel is the rim only).
    let mut wheel: Option<(Section, i32)> = None;
    let mut wheel_main: Option<&Section> = None;

    let mut stub_sections: Vec<String> = Vec::new();
    for s in &main.sections {
        let lname = s.name.to_lowercase();
        if !is_stock(&lname, kit) || is_placeholder(s) {
            continue;
        }
        // Prefer the high-detail sibling's LOD0 geometry.
        let hi = lod0.as_ref().and_then(|c| c.sections.iter().find(|x| x.name.eq_ignore_ascii_case(&s.name))).filter(|h| !is_placeholder(h));
        // Some `_lod0.carbin`s are stubs (VW_Beetle_04: 2-4 triangles per section): keep the main LOD1
        // unless the LOD0 sibling has at least half its triangles.
        let tris = |v: &[&Subsection]| v.iter().map(|x| x.indices.len() / 3).sum::<usize>();
        let (sec, subs) = match hi {
            Some(h) if h.subsections.iter().any(|x| x.lod == 0) && tris(&best_lod(h)) * 2 >= tris(&best_lod(s)) => (h, best_lod(h)),
            Some(h) if h.subsections.iter().any(|x| x.lod == 0) => {
                stub_sections.push(s.name.clone());
                (s, best_lod(s))
            }
            _ => (s, best_lod(s)),
        };
        if lname == "wheel" {
            wheel = subs.first().map(|x| (sec.clone(), x.lod));
            wheel_main = Some(s);
            continue;
        }
        if let Some(mesh) = g.mesh(&s.name, sec, &subs, &atlases, paint) {
            children.push(g.node(json!({"name": s.name, "mesh": mesh})));
        }
    }

    // Sections only `_lod0.carbin` has: at LOD1 the body (or a neighbour) carries them, at LOD0 they are split out
    // (tail lights of FOR_MustangBOSS429_70 / DOD_ChargerRT_69 / SHE_CobraDaytona_65, lamp glass of
    // NIS_SkylineGTRVSpecII_02 / LOT_Exige240_06 / BMW_1M_11, FOR_GT40_66 + ASC_KZ1R_12 windows, F40 Competizione
    // bumper frames; car_survey LOD0ONLY). Only when the LOD0 body is drawn (else the LOD1 body already has them),
    // and never the 2-8 triangle stubs. FH1_LOD0_ONLY=0 = old behaviour (main carbin sections only).
    let body_is_lod0 = !stub_sections.iter().any(|s| s.eq_ignore_ascii_case("body"))
        && lod0.as_ref().is_some_and(|c| c.sections.iter().any(|x| x.name.eq_ignore_ascii_case("body") && !is_placeholder(x)));
    if body_is_lod0 && std::env::var("FH1_LOD0_ONLY").as_deref() != Ok("0") {
        for h in lod0.as_ref().map(|c| c.sections.as_slice()).unwrap_or_default() {
            let lname = h.name.to_lowercase();
            if main.sections.iter().any(|s| s.name.eq_ignore_ascii_case(&h.name)) || lname == "wheel" || !is_stock(&lname, kit) || is_placeholder(h) {
                continue;
            }
            let subs: Vec<&Subsection> = h.subsections.iter().filter(|x| x.lod == 0).collect();
            if subs.iter().map(|x| x.indices.len() / 3).sum::<usize>() < 16 {
                continue;
            }
            if let Some(mesh) = g.mesh(&h.name, h, &subs, &atlases, paint) {
                children.push(g.node(json!({"name": h.name, "mesh": mesh})));
            }
        }
    }

    // Wheels. Geometry comes from the stock rim's carbin (fallback: the car's own wheel section). The rim
    // model is built at one size (usually one of the car's axles); each axle gets its own copy scaled to the
    // gamedb tyre and rim sizes (INFERRED, see docs/WHEELS.md). The stored wheel is the left-side one;
    // the right-side nodes mirror it.
    let mut hubs = Vec::new();
    let (mut wheel_radius, mut wheel_width) = (0.33f32, 0.25f32);
    let car_wheel = wheel_main.map(|m| (m, wheel.as_ref().map(|(s, _)| s)));
    let geo = match &rim {
        Some((sec, _)) => WheelGeo::new(sec, Some(sec)),
        None => car_wheel.and_then(|(m, hi)| WheelGeo::new(m, hi)),
    };
    if let Some(geo) = geo {
        let mut meshes = [Vec::new(), Vec::new()];
        let mut axles_done: Vec<(usize, [f32; 3])> = Vec::new();
        for (a, axle) in wheels.axles.iter().enumerate() {
            let fit = geo.fit_to(axle);
            // Identical axles share their meshes.
            if let Some(&(b, _)) = axles_done.iter().find(|(_, f)| *f == [axle.tyre_radius, axle.tyre_width, axle.rim_radius]) {
                meshes[a] = meshes[b].clone();
                continue;
            }
            axles_done.push((a, [axle.tyre_radius, axle.tyre_width, axle.rim_radius]));
            let tag = if a == 0 { "F" } else { "R" };
            let rim_xf = |p: [f32; 3]| fit.rim(geo.rim_fit.apply(p));
            let tyre_xf = |p: [f32; 3]| fit.tyre(p);
            meshes[a] = [
                g.mesh_xf(&format!("rim_{tag}"), geo.rim_sec, &geo.rim_subs, &atlases, paint, &rim_xf),
                g.mesh_xf(&format!("tyre_{tag}"), geo.tyre_sec, &geo.tyre_subs, &atlases, paint, &tyre_xf),
            ]
            .into_iter()
            .flatten()
            .collect();
        }
        wheel_radius = wheels.axles[0].tyre_radius;
        wheel_width = wheels.axles[0].tyre_width;
        for (i, corner) in CORNERS.iter().enumerate() {
            let front = i < 2;
            let left = i % 2 == 0;
            let axle = &wheels.axles[if front { 0 } else { 1 }];
            // TrackOuter is measured across the outside of the tyres.
            let track = if front { front_track } else { rear_track };
            // Hubs in the physics frame (wheelbase-centred, like MAXData); the glTF root is offset by -centre
            // so the body mesh lines up with them.
            let x = (track * 0.5 - axle.tyre_width * 0.5) * if left { -1.0 } else { 1.0 };
            let z = if front { -wheelbase * 0.5 } else { wheelbase * 0.5 };
            let y = hub_height[if front { 0 } else { 1 }];
            hubs.push([x, y, z]);
            let [x, y, z] = [x + wheels.centre[0], y + wheels.centre[1], z + wheels.centre[2]];
            let parts: Vec<usize> = meshes[if front { 0 } else { 1 }].clone().into_iter().map(|mesh| g.node(json!({"mesh": mesh}))).collect();
            let mut node = json!({"name": format!("wheel_{corner}"), "children": parts, "translation": [x, y, z]});
            if !left {
                node["scale"] = json!([-1.0, 1.0, 1.0]);
            }
            children.push(g.node(node));
        }
    }

    // Brake rotors and calipers: per-corner carbins positioned relative to the hub.
    for (i, corner) in CORNERS.iter().enumerate() {
        let Some(hub) = hubs.get(i).copied() else { break };
        for part in ["rotor", "caliper"] {
            let Some(d) = read(&format!("{media}_{part}{corner}_LOD0.carbin"))? else { continue };
            let Ok(c) = carbin::parse(&d) else { continue };
            for s in c.sections.iter().filter(|s| !is_placeholder(s)) {
                if let Some(mesh) = g.mesh(&format!("{part}_{corner}"), s, &best_lod(s), &atlases, paint) {
                    let at = [hub[0] + wheels.centre[0], hub[1] + wheels.centre[1], hub[2] + wheels.centre[2]];
                    children.push(g.node(json!({"name": format!("{part}_{corner}"), "mesh": mesh, "translation": at})));
                }
            }
        }
    }

    if !stub_sections.is_empty() {
        println!("[cars] {media}: _lod0 stubs, kept LOD1 for {}", stub_sections.join(" "));
    }
    // Mesh space -> physics frame: the meshes' origin is the body's bottom-centre, the physics (MAXData,
    // model.json hubs) is centred on the wheelbase bottom-centre.
    let offset = wheels.centre.map(|v| -v);
    let root = g.node(json!({"name": media, "children": children, "translation": offset}));
    let (flipped, triangles) = (g.flipped, g.triangles);
    g.write(out, root)?;

    let info = json!({
        "media_name": media,
        "forward": "-Z",
        "wheel_radius": wheel_radius,
        "wheel_width": wheel_width,
        "lod0_stub_sections": stub_sections,
        // Stock body-kit letters that aren't `a` (fh1-render car.rs is_stock should follow these).
        "kit": kit.0.iter().filter(|(_, l)| *l != 'a').map(|(s, l)| (s.to_string(), json!(l.to_string()))).collect::<serde_json::Map<_, _>>(),
        "mesh_offset": offset,
        // Stock paint for the game's car shaders (fh1-render car::load_body): Combo_Colors RGB, Metallic, Sequence.
        "paint": {"rgb": paint.rgb, "metallic": paint.metallic, "sequence": paint.sequence},
        "rim": if rim.is_some() { json!(wheels.rim_media) } else { Value::Null },
        "axles": wheels.axles.iter().map(|a| json!({"tyre_radius": a.tyre_radius, "tyre_width": a.tyre_width, "rim_radius": a.rim_radius})).collect::<Vec<_>>(),
        "hubs": CORNERS.iter().zip(&hubs).map(|(c, h)| json!({"corner": c, "position": h})).collect::<Vec<_>>(),
        "winding_flipped": flipped,
        "triangles": triangles,
    });
    std::fs::write(out.join("model.json"), serde_json::to_vec_pretty(&info)?)?;
    Ok(info)
}

/// `PaintScale` of the rim techniques (`rim_V2`, `inner_rim_V2`...) in shared `ShaderSettings/Tire/Normal.xml`;
/// multiplies the rim texture in the rim PS (linear). TintColor there is white.
const RIM_PAINT_SCALE: f32 = 0.65;

/// The rim's carbin section and its decoded `wheel_LOD0.xds` (or `wheel.xds`).
pub(crate) fn read_rim(zip: &Path, media: &str) -> Result<Option<(Section, Option<(u32, u32, Vec<u8>)>)>> {
    let mut ar = Archive::open(zip)?;
    let mut read = |name: &str| -> Result<Option<Vec<u8>>> {
        match ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned() {
            Some(e) => Ok(Some(ar.read(&e)?)),
            None => Ok(None),
        }
    };
    let Some(c) = read(&format!("{media}.carbin"))?.and_then(|d| carbin::parse(&d).ok()) else { return Ok(None) };
    let Some(sec) = c.sections.into_iter().find(|s| s.name.eq_ignore_ascii_case("wheel")) else { return Ok(None) };
    let mut tex = None;
    for f in ["wheel_LOD0.xds", "wheel.xds"] {
        if let Some(d) = read(f)? {
            let (_, img) = xds::decode_base(&d)?;
            tex = Some((img.width, img.height, xds::to_rgba8(&img)?));
            break;
        }
    }
    Ok(Some((sec, tex)))
}

fn is_tyre(name: &str) -> bool {
    name.to_lowercase().contains("tire")
}

/// Motion-blur shells (`blur_rim`, `blur_lip`, `chrome_blur_*`): the game fades them in with wheel speed.
fn is_blur(name: &str) -> bool {
    name.to_lowercase().contains("blur")
}

/// Per-axis linear map `p -> p * scale + add`.
#[derive(Clone, Copy)]
pub(crate) struct AxisMap {
    scale: [f32; 3],
    add: [f32; 3],
}

impl AxisMap {
    const ID: AxisMap = AxisMap { scale: [1.0; 3], add: [0.0; 3] };
    pub(crate) fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        [p[0] * self.scale[0] + self.add[0], p[1] * self.scale[1] + self.add[1], p[2] * self.scale[2] + self.add[2]]
    }
}

/// Bounding box of subsections' positions (section offset included).
fn bbox(s: &Section, subs: &[&Subsection]) -> Option<([f32; 3], [f32; 3])> {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for sub in subs {
        let pool = s.vertices_for(sub);
        for &i in &sub.indices {
            let p = pool[i as usize].position;
            for k in 0..3 {
                lo[k] = lo[k].min(p[k] + s.offset[k]);
                hi[k] = hi[k].max(p[k] + s.offset[k]);
            }
        }
    }
    (lo[0] <= hi[0]).then_some((lo, hi))
}

/// Min/max distance from the wheel axis (X) and the X extent of subsections (None when there are none).
fn radial(s: &Section, subs: &[&Subsection]) -> Option<(f32, f32, f32, f32)> {
    let (mut rmin, mut rmax, mut xmin, mut xmax) = (f32::MAX, 0.0f32, f32::MAX, f32::MIN);
    for sub in subs {
        let pool = s.vertices_for(sub);
        for &i in &sub.indices {
            let p = pool[i as usize].position;
            let (x, y, z) = (p[0] + s.offset[0], p[1] + s.offset[1], p[2] + s.offset[2]);
            let r = (y * y + z * z).sqrt();
            rmin = rmin.min(r);
            rmax = rmax.max(r);
            xmin = xmin.min(x);
            xmax = xmax.max(x);
        }
    }
    (rmax > 0.0).then_some((rmin, rmax, xmin, xmax))
}

/// A wheel model: tyre and rim subsections, plus the correction for the rim's LOD0 pool.
///
/// The LOD0 pool's decoded positions are wrong on rims: `carbin` remaps each pool from its own bbox
/// onto the section bounds, but a rim's bounds include the tyre, which only the LOD pool holds, so the
/// LOD0 rim comes out stretched to the tyre (rim zips) or shrunk (`_lod0.carbin` wheels: 0.193 m rims
/// inside 0.26 m tyre beads on ~60 cars; wheel_survey example). The LOD1 rim and the tyre share one pool and
/// agree, so the LOD0 rim is mapped per axis onto the LOD1 rim's bounding box.
pub(crate) struct WheelGeo<'a> {
    tyre_sec: &'a Section,
    tyre_subs: Vec<&'a Subsection>,
    pub(crate) rim_sec: &'a Section,
    pub(crate) rim_subs: Vec<&'a Subsection>,
    pub(crate) rim_fit: AxisMap,
    /// The game's model measures (82DA1CF8): tyre diameter, rim seat diameter, tyre width.
    pub(crate) model_tyre_d: f32,
    pub(crate) model_rim_d: f32,
    pub(crate) model_width: f32,
}

impl<'a> WheelGeo<'a> {
    /// `lod`: the section with the LOD pool (tyre + LOD1-5 rim); `hi`: the section holding the LOD0 rim.
    pub(crate) fn new(lod: &'a Section, hi: Option<&'a Section>) -> Option<Self> {
        let at_min = |s: &'a Section, f: &dyn Fn(&Subsection) -> bool| -> Vec<&'a Subsection> {
            let min = s.subsections.iter().filter(|x| f(x)).map(|x| x.lod).min();
            s.subsections.iter().filter(|x| f(x) && Some(x.lod) == min).collect()
        };
        let tyre_subs = at_min(lod, &|x| x.lod >= 1 && is_tyre(&x.name));
        // Measured like the game (82DA1CF8) on the LOD1 meshes (its "set 1"; INFERRED to be LOD1): max radius of
        // the rim-classified parts (blur shells included) and of the tyre parts, and the tyre's X extent.
        let (_, tyre_r, txmin, txmax) = radial(lod, &at_min(lod, &|x| x.lod >= 1 && game_is_tyre(&x.name)))?;
        let (_, rim_r, _, _) = radial(lod, &at_min(lod, &|x| x.lod >= 1 && game_is_rim(&x.name)))?;
        let ref_subs = at_min(lod, &|x| x.lod >= 1 && !is_tyre(&x.name) && !is_blur(&x.name));
        let mut hi_subs: Vec<&'a Subsection> = hi
            .map(|h| h.subsections.iter().filter(|x| x.lod == 0 && !is_tyre(&x.name) && !is_blur(&x.name)).collect())
            .unwrap_or_default();
        // Some rims ship a placeholder LOD0 (VW_Beetle_04: a 3-triangle `rim`); keep LOD1 for those.
        let tris = |v: &[&Subsection]| v.iter().map(|x| x.indices.len() / 3).sum::<usize>();
        if tris(&hi_subs) * 2 < tris(&ref_subs) {
            hi_subs.clear();
        }
        let (rim_sec, rim_subs, rim_fit) = match (hi, bbox(lod, &ref_subs)) {
            (Some(h), Some((rlo, rhi))) if !hi_subs.is_empty() => {
                let (lo, hi_) = bbox(h, &hi_subs)?;
                let mut m = AxisMap::ID;
                for k in 0..3 {
                    let span = hi_[k] - lo[k];
                    if span > 1e-4 {
                        m.scale[k] = (rhi[k] - rlo[k]) / span;
                    }
                    // Positions passed to `apply` already include the section offset.
                    m.add[k] = rlo[k] - lo[k] * m.scale[k];
                }
                (h, hi_subs, m)
            }
            _ => (lod, ref_subs, AxisMap::ID),
        };
        if rim_subs.is_empty() {
            return None;
        }
        Some(Self {
            tyre_sec: lod,
            tyre_subs,
            rim_sec,
            rim_subs,
            rim_fit,
            model_tyre_d: 2.0 * tyre_r,
            // The rim's outer lip sits half an inch outside the seat.
            model_rim_d: 2.0 * (rim_r - 0.0127),
            model_width: txmax - txmin,
        })
    }

    /// The game's `wheelScale` (VS c59), as 82D921E0 computes it from the gamedb sizes (VERIFIED):
    /// x = width / model width, y = rim diameter / model rim diameter, z = tyre diameter / model tyre diameter,
    /// w = (split radius)^2 with the split 30% of the way up the model's sidewall. The rim VS scales X by x and
    /// the radius by y; the tyre VS scales X by x and the radius by z beyond the split, y inside it.
    fn fit_to(&self, axle: &Axle) -> AxleFit {
        let ratio = |t: f32, m: f32| if m > 1e-4 { t / m } else { 1.0 };
        let split = (self.model_rim_d + 0.3 * (self.model_tyre_d - self.model_rim_d)) * 0.5;
        AxleFit {
            x: ratio(axle.tyre_width, self.model_width),
            y: ratio(2.0 * axle.rim_radius, self.model_rim_d),
            z: ratio(2.0 * axle.tyre_radius, self.model_tyre_d),
            split2: split * split,
        }
    }
}

struct AxleFit {
    x: f32,
    y: f32,
    z: f32,
    split2: f32,
}

impl AxleFit {
    fn rim(&self, p: [f32; 3]) -> [f32; 3] {
        [p[0] * self.x, p[1] * self.y, p[2] * self.y]
    }
    fn tyre(&self, p: [f32; 3]) -> [f32; 3] {
        let k = if p[1] * p[1] + p[2] * p[2] >= self.split2 { self.z } else { self.y };
        [p[0] * self.x, p[1] * k, p[2] * k]
    }
}

/// The game's rim / tyre part names (82D92020 / 82D91EE0, VERIFIED): prefix matches, plus `rim_` / `tire_` anywhere.
fn game_is_rim(name: &str) -> bool {
    const P: [&str; 13] = ["rim", "ghostRim", "inner_rim", "outer_rim", "chrome_rim", "chrome_blur_rim", "chrome_blur_lip", "wheel_emblem", "wheel_black", "blur_rim", "blur_lip", "dropShadowRim", "geoShadowRim"];
    P.iter().any(|p| name.starts_with(p)) || name.contains("rim_")
}

fn game_is_tyre(name: &str) -> bool {
    const P: [&str; 6] = ["tire", "dropShadowTire", "sidewall", "tread", "scaling_text", "wheel_black"];
    !game_is_rim(name)
        && (P.iter().any(|p| name.starts_with(p)) || ["ghostTire", "geoShadowTire", "tire_", "sidewall", "tread"].iter().any(|p| name.contains(p)))
}

/// Artist placeholders: an 8-triangle 0.2 m cube whose subsections are named `<section>_LOD<n>`. The trucks
/// (KEN_T440_12, BUS_TOUR_12) ship a `_lod0.carbin` made only of these, plus a few in the main carbin.
pub(crate) fn is_placeholder(s: &Section) -> bool {
    let name = s.name.to_lowercase();
    !s.subsections.is_empty()
        && s.subsections.iter().all(|x| {
            let n = x.name.to_lowercase();
            n.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with("_lod"))
        })
}

/// Stock parts only: drop body-kit letters other than the stock one ([`Kit`], `a` on all but one car), race parts
/// and lexan windows.
fn is_stock(n: &str, kit: &Kit) -> bool {
    if n.starts_with("lexan") || n.starts_with("cage") || n.ends_with("race") {
        return false;
    }
    const STEMS: [&str; 10] = ["bumperf", "bumperr", "hood", "wing", "skirtl", "skirtr", "exhaustl", "exhaustr", "exhaust", "undercarriage"];
    for stem in STEMS {
        if let Some(rest) = n.strip_prefix(stem) {
            let mut ch = rest.chars();
            if let Some(letter) = ch.next().filter(|c| c.is_ascii_lowercase()) {
                let tail = ch.as_str();
                if tail.is_empty() || tail.starts_with('_') {
                    return letter == kit.letter(stem);
                }
            }
        }
    }
    true
}

/// Subsections of the most detailed LOD present.
pub(crate) fn best_lod(s: &Section) -> Vec<&Subsection> {
    let Some(min) = s.subsections.iter().map(|x| x.lod).min() else { return Vec::new() };
    s.subsections.iter().filter(|x| x.lod == min).collect()
}

struct Material {
    color: [f32; 4],
    metallic: f32,
    roughness: f32,
    atlas: Option<&'static str>,
}

/// Techniques whose pixel shader samples `LightsSampler` (the car's `lights_LOD0` atlas), from the game's
/// car shader library `media/shaders/cars/shaders_v16.fxobj` (VERIFIED: per-technique PS sampler tables).
const LIGHT_TECHNIQUES: [&str; 55] = [
    "detail_glass_red", "detail_glass_clear", "detail_glass_amber", "head_light", "tail_light", "tail_light_lod0", "tail_light_on",
    "tail_light2", "reverse_light", "reverse_light_lod0", "textured_reflector", "xenonhead", "hidhead", "oldhead", "fullbeam", "fogwhite",
    "fogred", "taillightst", "taillight2s", "brkreverse", "linred", "rinred", "indicator_left", "indicator_right", "drlwhite",
    "drlwhiteswitch", "drlred", "drlorange", "drlbrakered", "drlbrakeorange", "drlinlorange", "drlinrorange", "drlinlred", "drlinrred",
    "drlinlwhite", "drlinrwhite", "numplate", "brinl", "brinr", "lindtailside", "rindtailside", "slorange", "slinlorange", "slinrorange",
    "lights_glass", "lights_gls_noemit", "lights_gls_tail_light", "lights_gls_reverse_light", "lights_gls_indicator_left",
    "lights_gls_indicator_right", "lights_gls_hidhead", "lights_gls_oldhead", "lights_gls_taillightst", "lights_gls_taillight2s", "reflector",
];

/// Material (= technique, docs/SHADERS.md) -> look. Which texture each technique samples comes from the
/// shaders_v16.fxobj sampler tables: lights -> lights atlas; grille/bumper frame/undercarriage/carbon -> their
/// Shared.zip textures (`textures.xml`; their UVs tile or span 0..1 on their own); chrome, black, bottom,
/// mirrors, glass -> no base texture; everything with `baseSampler` -> the exterior atlas (nodamage).
fn material_for(name: &str, paint: &Paint) -> Material {
    let n = name.to_lowercase();
    let has = |k: &str| n.contains(k);
    let m = |color: [f32; 4], metallic, roughness, atlas| Material { color, metallic, roughness, atlas };
    // Lamp lenses: pass 0 of detail_glass_* / lights_glass draws the lamp image from the lights atlas
    // (R = lerp(DarkValues, LightValues, tex.r) for the lamp on/off state, G/B = tex.g/b; PS 287); pass 1 lays
    // the GlassColor tint over it. Drawn opaque with the atlas here (blend states are set by code: INFERRED).
    if n.starts_with("detail_glass") || n == "lights_glass" {
        return m([1.0, 1.0, 1.0, 1.0], 0.0, 0.2, Some("lights"));
    }
    if (has("glass") || has("window")) && !n.starts_with("lights_gls") {
        let c = if has("red") { [0.55, 0.02, 0.02, 0.6] } else if has("clear") { [0.9, 0.9, 0.9, 0.25] } else if has("black") { [0.02, 0.02, 0.02, 0.9] } else if has("amber") { [0.8, 0.35, 0.02, 0.6] } else { [0.08, 0.1, 0.11, 0.45] };
        // Not mirror-smooth: at 0.05 the sun's highlight is a pinpoint bright enough for the bloom to spread
        // into a white blob over the windscreen (festival shots, 2026-10-04).
        return m(c, 0.0, 0.4, None);
    }
    if n == "body" || n.starts_with("paint") || n.starts_with("body") {
        let [_, r, gg, b] = paint.rgb.to_be_bytes();
        let lin = |v: u8| (v as f32 / 255.0).powf(2.2);
        return m([lin(r), lin(gg), lin(b), 1.0], if paint.metallic { 0.6 } else { 0.0 }, 0.45, Some("exterior"));
    }
    if LIGHT_TECHNIQUES.contains(&n.as_str()) {
        return m([1.0, 1.0, 1.0, 1.0], 0.0, 0.25, Some("lights"));
    }
    match n.as_str() {
        "grille1" | "grille1_alpha" => return m([1.0, 1.0, 1.0, 1.0], 0.3, 0.5, Some("grille1")),
        "grille2" | "grille2_alpha" => return m([1.0, 1.0, 1.0, 1.0], 0.3, 0.5, Some("grille2")),
        "bumper_frame" => return m([1.0, 1.0, 1.0, 1.0], 0.0, 0.6, Some("bumper_frame")),
        "undercarriage" => return m([1.0, 1.0, 1.0, 1.0], 0.0, 0.8, Some("undercarriage")),
        _ => {}
    }
    if has("carbon") {
        return m([1.0, 1.0, 1.0, 1.0], 0.2, 0.3, Some("carbon"));
    }
    if has("chrome") || has("mirror") {
        // Chrome is env-map-only in the game; with plain PBR a 0.95 mirror turns the sun into bloom blobs.
        return m([0.6, 0.6, 0.6, 1.0], 1.0, 0.45, None);
    }
    if has("tire") || has("rubber") {
        return m([0.04, 0.04, 0.04, 1.0], 0.0, 0.9, None);
    }
    if n == "black" || n.starts_with("black_") || n.starts_with("bottom") || n == "wheel_black" {
        return m([0.03, 0.03, 0.03, 1.0], 0.0, 0.7, None);
    }
    if has("interior") || has("seat") || has("leather") || has("dash") || has("cloth") {
        return m([1.0, 1.0, 1.0, 1.0], 0.0, 0.8, Some("interior"));
    }
    m([1.0, 1.0, 1.0, 1.0], 0.0, 0.5, Some("exterior"))
}

/// Wheel subsections: rim parts sample the rim texture times PaintScale, the tyre the shared `tireA0` texture,
/// `chrome_rim`/`chrome_blur_*` are env-map-only techniques (no base texture in shaders_v16).
fn wheel_material(name: &str, atlases: &HashMap<&str, String>, textured: bool) -> Material {
    let n = name.to_lowercase();
    let m = |color: [f32; 4], metallic, roughness, atlas| Material { color, metallic, roughness, atlas };
    if n.contains("tire") {
        return match textured && atlases.contains_key("tire") {
            true => m([1.0, 1.0, 1.0, 1.0], 0.0, 0.9, Some("tire")),
            false => m([0.04, 0.04, 0.04, 1.0], 0.0, 0.9, None),
        };
    }
    if n.contains("black") {
        return m([0.03, 0.03, 0.03, 1.0], 0.0, 0.6, None);
    }
    if n.contains("chrome") {
        return m([0.6, 0.6, 0.6, 1.0], 1.0, 0.45, None);
    }
    let k = RIM_PAINT_SCALE;
    match textured && atlases.contains_key("wheel") {
        true => m([k, k, k, 1.0], 0.3, 0.35, Some("wheel")),
        false => m([k * 0.7, k * 0.7, k * 0.7, 1.0], 0.3, 0.35, None),
    }
}

#[derive(Default)]
pub(crate) struct Gltf {
    bin: Vec<u8>,
    buffer_views: Vec<Value>,
    accessors: Vec<Value>,
    meshes: Vec<Value>,
    nodes: Vec<Value>,
    materials: Vec<Value>,
    material_ids: HashMap<String, usize>,
    images: Vec<Value>,
    image_ids: HashMap<String, usize>,
    /// Triangles whose winding we reversed to make them counter-clockwise / outward.
    flipped: bool,
    winding_decided: bool,
    triangles: usize,
}

impl Gltf {
    pub(crate) fn node(&mut self, n: Value) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    fn view(&mut self, bytes: &[u8], target: u32) -> usize {
        while self.bin.len() % 4 != 0 {
            self.bin.push(0);
        }
        let offset = self.bin.len();
        self.bin.extend_from_slice(bytes);
        self.buffer_views.push(json!({"buffer": 0, "byteOffset": offset, "byteLength": bytes.len(), "target": target}));
        self.buffer_views.len() - 1
    }

    fn accessor(&mut self, view: usize, ctype: u32, count: usize, ty: &str, minmax: Option<([f32; 3], [f32; 3])>) -> usize {
        let mut a = json!({"bufferView": view, "componentType": ctype, "count": count, "type": ty});
        if let Some((lo, hi)) = minmax {
            a["min"] = json!(lo);
            a["max"] = json!(hi);
        }
        self.accessors.push(a);
        self.accessors.len() - 1
    }

    fn material(&mut self, name: &str, atlases: &HashMap<&str, String>, paint: &Paint, textured: bool, wheel: bool) -> usize {
        let mut mat = if wheel { wheel_material(name, atlases, textured) } else { material_for(name, paint) };
        if !textured {
            mat.atlas = None;
        }
        let key = format!("{name}|{}|{wheel}", mat.atlas.unwrap_or("-"));
        if let Some(&id) = self.material_ids.get(&key) {
            return id;
        }
        let mut pbr = json!({"baseColorFactor": mat.color, "metallicFactor": mat.metallic, "roughnessFactor": mat.roughness});
        if let Some(png) = mat.atlas.and_then(|a| atlases.get(a)) {
            let img = *self.image_ids.entry(png.clone()).or_insert_with(|| {
                self.images.push(json!({"uri": png}));
                self.images.len() - 1
            });
            pbr["baseColorTexture"] = json!({"index": img});
        }
        let mut m = json!({"name": name, "pbrMetallicRoughness": pbr, "doubleSided": false});
        // Grille meshes are cut out by the grille textures' alpha.
        if matches!(mat.atlas, Some("grille1" | "grille2")) && atlases.contains_key(mat.atlas.unwrap()) {
            m["alphaMode"] = json!("MASK");
            m["alphaCutoff"] = json!(0.5);
            m["doubleSided"] = json!(true);
        }
        if mat.color[3] < 1.0 {
            m["alphaMode"] = json!("BLEND");
            m["doubleSided"] = json!(true);
        }
        self.materials.push(m);
        self.material_ids.insert(key, self.materials.len() - 1);
        self.materials.len() - 1
    }

    /// One glTF mesh for a section: a primitive per subsection, vertices compacted per primitive
    /// (subsections share pools but each has its own UV transform).
    pub(crate) fn mesh(&mut self, name: &str, s: &Section, subs: &[&Subsection], atlases: &HashMap<&str, String>, paint: &Paint) -> Option<usize> {
        self.mesh_impl(name, s, subs, atlases, paint, None)
    }

    /// A wheel mesh: positions (section offset included) pass through `xf`; wheel materials.
    pub(crate) fn mesh_xf(&mut self, name: &str, s: &Section, subs: &[&Subsection], atlases: &HashMap<&str, String>, paint: &Paint, xf: &dyn Fn([f32; 3]) -> [f32; 3]) -> Option<usize> {
        self.mesh_impl(name, s, subs, atlases, paint, Some(xf))
    }

    fn mesh_impl(&mut self, name: &str, s: &Section, subs: &[&Subsection], atlases: &HashMap<&str, String>, paint: &Paint, xf: Option<&dyn Fn([f32; 3]) -> [f32; 3]>) -> Option<usize> {
        let mut prims = Vec::new();
        for sub in subs {
            let pool = s.vertices_for(sub);
            if sub.indices.is_empty() || pool.is_empty() {
                continue;
            }
            let mut remap: HashMap<u32, u32> = HashMap::new();
            let mut verts = Vec::new();
            let mut idx: Vec<u32> = sub
                .indices
                .iter()
                .map(|&i| {
                    *remap.entry(i).or_insert_with(|| {
                        verts.push(pool[i as usize]);
                        verts.len() as u32 - 1
                    })
                })
                .collect();
            let pos: Vec<[f32; 3]> = verts
                .iter()
                .map(|v| [v.position[0] + s.offset[0], v.position[1] + s.offset[1], v.position[2] + s.offset[2]])
                .map(|p| xf.map_or(p, |f| f(p)))
                .collect();

            // Winding: compare geometric normals with the stored (quaternion) normals once, on the
            // first sizeable primitive, and apply the same decision to every car part.
            if !self.winding_decided && idx.len() >= 300 {
                let mut agree = 0i64;
                for t in idx.chunks_exact(3) {
                    let n = tri_normal(pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]);
                    let q = verts[t[0] as usize].normal;
                    agree += if n[0] * q[0] + n[1] * q[1] + n[2] * q[2] >= 0.0 { 1 } else { -1 };
                }
                self.flipped = agree < 0;
                self.winding_decided = true;
            }
            if self.flipped {
                for t in idx.chunks_exact_mut(3) {
                    t.swap(1, 2);
                }
            }

            let normals = smooth_normals(&pos, &idx);
            let (ox, sx, oy, sy) = (sub.uv_transform[0], sub.uv_transform[1], sub.uv_transform[2], sub.uv_transform[3]);
            // A zero scale is the game's own data: its VS does uv * scale + offset, so the part samples one texel
            // (or one row/column) of the atlas at the offset: a flat colour, not "untextured" (817 subsections, e.g.
            // `frame`, `matte_colors`, `plastic2`, which came out white). FH1_UV_ZERO_TEX=0 = old (no texture).
            let textured = (sx.abs() > 1e-5 && sy.abs() > 1e-5) || std::env::var("FH1_UV_ZERO_TEX").as_deref() != Ok("0");
            let uvs: Vec<[f32; 2]> = verts.iter().map(|v| [v.uv0[0] * sx + ox, v.uv0[1] * sy + oy]).collect();

            let mut lo = [f32::MAX; 3];
            let mut hi = [f32::MIN; 3];
            for p in &pos {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
            let v = self.view(bytemuck_f32(pos.as_flattened()), 34962);
            let a_pos = self.accessor(v, 5126, pos.len(), "VEC3", Some((lo, hi)));
            let v = self.view(bytemuck_f32(normals.as_flattened()), 34962);
            let a_nrm = self.accessor(v, 5126, normals.len(), "VEC3", None);
            let v = self.view(bytemuck_f32(uvs.as_flattened()), 34962);
            let a_uv = self.accessor(v, 5126, uvs.len(), "VEC2", None);
            let ib: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
            let v = self.view(&ib, 34963);
            let a_idx = self.accessor(v, 5125, idx.len(), "SCALAR", None);
            let mat = self.material(&sub.name, atlases, paint, textured, xf.is_some());
            self.triangles += idx.len() / 3;
            prims.push(json!({
                "attributes": {"POSITION": a_pos, "NORMAL": a_nrm, "TEXCOORD_0": a_uv},
                "indices": a_idx,
                "material": mat,
            }));
        }
        if prims.is_empty() {
            return None;
        }
        self.meshes.push(json!({"name": name, "primitives": prims}));
        Some(self.meshes.len() - 1)
    }

    pub(crate) fn write(self, out: &Path, root: usize) -> Result<()> {
        std::fs::write(out.join("model.bin"), &self.bin)?;
        let samplers = json!([{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}]);
        let textures: Vec<Value> = (0..self.images.len()).map(|i| json!({"source": i, "sampler": 0})).collect();
        let doc = json!({
            "asset": {"version": "2.0", "generator": "fh1setup"},
            "scene": 0,
            "scenes": [{"nodes": [root]}],
            "nodes": self.nodes,
            "meshes": self.meshes,
            "materials": self.materials,
            "textures": textures,
            "images": self.images,
            "samplers": samplers,
            "accessors": self.accessors,
            "bufferViews": self.buffer_views,
            "buffers": [{"uri": "model.bin", "byteLength": self.bin.len()}],
        });
        std::fs::write(out.join("model.gltf"), serde_json::to_vec(&doc)?)?;
        Ok(())
    }
}

fn bytemuck_f32(v: &[f32]) -> &[u8] {
    // f32 has no padding and any alignment of u8 is fine; little-endian hosts only (Windows x64).
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

fn tri_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]]
}

/// Area-weighted vertex normals.
fn smooth_normals(pos: &[[f32; 3]], idx: &[u32]) -> Vec<[f32; 3]> {
    let mut n = vec![[0.0f32; 3]; pos.len()];
    for t in idx.chunks_exact(3) {
        let f = tri_normal(pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]);
        for &i in t {
            for k in 0..3 {
                n[i as usize][k] += f[k];
            }
        }
    }
    for v in &mut n {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        *v = if l > 1e-12 { [v[0] / l, v[1] / l, v[2] / l] } else { [0.0, 1.0, 0.0] };
    }
    n
}

pub fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}
