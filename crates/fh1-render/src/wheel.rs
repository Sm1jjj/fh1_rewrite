//! Car wheels, brake rotors and calipers through the game's car shaders (docs/WHEELS.md "Wheels in the engine").
//!
//! - Rims and tyres: the stock rim model `cars/wheels/<rim>/<rim>.fxcar` (model.json `rim`), LOD1 pool (rim + tyre;
//!   the game measures its "set 1" there, INFERRED), techniques `<material>_V2` (rim_V2 PS 465, tire_V2 PS 460).
//!   The VS sizes the model by `wheelScale` (c59), filled per axle from model.json `axles` with the game's formula
//!   (0x82D921E0 / 0x82DA1CF8, VERIFIED; docs/WHEELS.md).
//! - Rotors/calipers: `cars/<car>/fx/<car>_rotor|caliper<corner>_LOD0.fxcar`, techniques `rotor` / `caliper`, the car's
//!   own exterior atlas.
//!
//! The meshes are spawned under the glTF nodes `wheel_XX` (which the vehicle code spins and steers), `rotor_XX` and
//! `caliper_XX`; the glTF meshes they replace are hidden. Needs [`crate::car::FxCarBody`] on the car's glTF holder.
//! Default on; `FH1_CARFX_WHEELS=0` keeps the glTF wheels; `FH1_CARFX_LOG=1` logs the parts and the c59 values.

use std::collections::HashSet;
use std::sync::Arc;

use bevy::camera::primitives::Aabb;
use bevy::prelude::*;

use crate::car::{parse_fxcar, subsection_mesh, CarShading, CarTextures, FxCarBody, FxCarSection, ShaderSettings, SH_COMPRESSION_DEFAULT};
use crate::car_material::FxCarMaterial;
use crate::car_shadow::FxWheelPart;
use crate::program::{Family, Program};
use crate::{FxCarGlobals, FxLibrary};

pub struct FxWheelPlugin;

impl Plugin for FxWheelPlugin {
    fn build(&self, app: &mut App) {
        // Default on (no measurable cost in FH1_WHEELS_AB runs); FH1_CARFX_WHEELS=0 = the glTF wheels.
        if std::env::var("FH1_CARFX_WHEELS").is_ok_and(|v| v == "0") || std::env::var("FH1_CARFX").is_ok_and(|v| v == "0") || std::env::var("FH1_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("remaster")) {
            return;
        }
        app.add_systems(Update, (load_fx_wheels, attach_fx_wheels).chain());
        if std::env::var("FH1_WHEELS_AB").is_ok_and(|v| v == "1") {
            app.add_systems(Update, wheels_ab);
        }
    }
}

/// Constants the CPU sets per wheel part (they are not in ShaderSettings, so without this they would be shared
/// globals and front/rear could not differ).
const WHEEL_CONSTANTS: [&str; 10] = [
    "wheelScale",
    "wheelMorph",
    "tireDeformation",
    "tireRotation",
    "tireDefConstants",
    "blurRimTexBlendAmounts",
    "blurRimGeomTexBlend",
    "TintColor",
    "caliperTintColor",
    "rotorHeat",
];

struct Part {
    mesh: Mesh,
    material: FxCarMaterial,
    aabb: Aabb,
    wheel: Option<FxWheelPart>,
}

/// Parts waiting for their glTF node (by name) to spawn.
#[derive(Component, Default)]
struct FxWheelsPending(Vec<(String, Vec<Part>)>);

fn set_named(m: &mut FxCarMaterial, program: &Program, name: &str, v: [f32; 4]) {
    for (n, stage, reg, _) in &program.named {
        if n == name {
            let file = if *stage == fh1_shaders::container::Stage::Vertex { &mut m.consts.vs } else { &mut m.consts.ps };
            if let Some(r) = file.get_mut(*reg as usize) {
                *r = Vec4::from_array(v);
            }
        }
    }
}

fn is_rim(m: &str) -> bool {
    const RIM: [&str; 13] = [
        "rim", "ghostRim", "inner_rim", "outer_rim", "chrome_rim", "chrome_blur_rim", "chrome_blur_lip", "wheel_emblem", "wheel_black", "blur_rim",
        "blur_lip", "dropShadowRim", "geoShadowRim",
    ];
    RIM.iter().any(|p| m.starts_with(p)) || m.contains("rim_")
}

fn is_tyre(m: &str) -> bool {
    ["tire", "dropShadowTire", "sidewall", "tread", "scaling_text"].iter().any(|p| m.starts_with(p)) || ["ghostTire", "geoShadowTire", "tire_"].iter().any(|p| m.contains(p))
}

/// Not drawn: the spoke blur shell (`blur_rim`, `chrome_blur_rim`) of a wheel that isn't blurring, and the shadow/ghost
/// techniques. The lip (`blur_lip`, `chrome_blur_lip`) IS drawn at rest: the game draws rim, inner_rim, blur_lip and tire
/// per wheel and no blur_rim (Pinyon captures expo_frame6631 / tod 08:00, VIP_Viper_13, blurRimGeomTexBlend 0).
fn skip_part(m: &str) -> bool {
    m.contains("blur_rim") || ["ghost", "dropShadow", "geoShadow", "SMask", "ShadowMap"].iter().any(|w| m.contains(w))
}

/// FH1_RIM_FIX=0: the old rim folder (always FH1's `cars/wheels`) and the uniform LOD0 rim refit.
fn rim_fix() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_RIM_FIX").map_or(true, |v| v != "0"))
}

/// Scale the packed X (Snorm16, `xyz·w`) of a car mesh's positions by `r` (r <= 1 in practice; clamped).
fn scale_packed_x(mesh: &mut Mesh, r: f32) {
    if let Some(bevy::mesh::VertexAttributeValues::Snorm16x4(v)) = mesh.attribute_mut(crate::material::ATTRIBUTE_CAR_POSITION) {
        for p in v.iter_mut() {
            p[0] = (p[0] as f32 * r).round().clamp(-32767.0, 32767.0) as i16;
        }
    }
}

fn pack_t_s(section: &FxCarSection) -> Aabb {
    let o = Vec3::from_array(section.offset);
    Aabb::from_min_max(o + Vec3::from_array(section.bounds_min), o + Vec3::from_array(section.bounds_max))
}

#[allow(clippy::too_many_arguments)]
fn load_fx_wheels(
    mut commands: Commands,
    added: Query<(Entity, &FxCarBody, Option<&crate::car::FxCarRim>), Added<FxCarBody>>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxCarGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
    env: Option<Res<crate::reflect::EnvCube>>,
) {
    let log = std::env::var_os("FH1_CARFX_LOG").is_some();
    let env_dynamic = env.as_ref().filter(|c| c.use_dynamic).map(|c| c.dynamic.clone());
    for (e, body, rim_override) in &added {
        let cars = body.assets.join("cars");
        let model: serde_json::Value = crate::files::read_json(&cars.join(&body.car).join("model.json")).unwrap_or_default();
        let read = |p: std::path::PathBuf| crate::files::read_to_string(&p);
        let shared = cars.join("shared/ShaderSettings");
        let Some(lib_bytes) = crate::files::read(&body.assets.join("shaders/media/Cars/shaders_v16.fxobj")) else { continue };
        let track = if crate::files::exists(&cars.join("cubemaps").join(format!("{}.dds", body.track))) { body.track.as_str() } else { "colorado" };
        let mut pending = FxWheelsPending::default();

        // ---- rims + tyres ----
        // Settings: Normal.xml, Tire/Normal.xml ("applied to ALL wheels and tires"), then the rim zip's own
        // RimShaderSettings.xml and ShaderSettings.xml (per-rim PaintScale/SpecularPower/FresnelIndex/FresnelScalar).
        // VERIFIED on Pinyon (VIP_Viper_13, expo_frame6631): rim_V2 FresnelIndex 1.35 = the rim's ShaderSettings.xml
        // (RimShaderSettings says 1.557), blur_lip_V2 SpecularPower 12.5, PaintScale 1.2; so ShaderSettings.xml is applied
        // after RimShaderSettings.xml (or alone). Both are absent from installs before fh1setup copies them (no-op then).
        // The Customize menu's aftermarket rim (FxCarRim) replaces the stock one.
        let rim = rim_override.map_or_else(|| model["rim"].as_str().unwrap_or(&body.car).to_string(), |r| r.0.clone());
        // The rim folder of the car's own game: `<cars>/wheels/<rim>` next to the car folder. Imported cars
        // (`../imported/<game>/cars/<media>`) used to read FH1's `cars/wheels/<rim>`: a same-named FH1 rim (~100 FH2 and
        // ~100 FM4 cars) or none (glTF wheels). FH1_RIM_FIX=0 = old.
        let rim_dir = if rim_fix() {
            cars.join(&body.car).parent().map_or_else(|| cars.clone(), |p| p.to_path_buf()).join("wheels").join(&rim)
        } else {
            cars.join("wheels").join(&rim)
        };
        let settings = ShaderSettings::chain(&[
            read(shared.join("Normal.xml")).as_deref(),
            read(shared.join("Tire/Normal.xml")).as_deref(),
            read(rim_dir.join("RimShaderSettings.xml")).as_deref(),
            read(rim_dir.join("ShaderSettings.xml")).as_deref(),
        ]);
        let rim_fx = crate::files::read(&rim_dir.join(format!("{rim}.fxcar"))).and_then(|b| parse_fxcar(&b));
        if let (Some(fx), Some(mut shading)) = (rim_fx, CarShading::new("shaders_v16", &lib_bytes, settings, &mut lib)) {
            widen_family(&mut shading);
            shading.sh_compression = fx.sh_compression.unwrap_or(SH_COMPRESSION_DEFAULT);
            // Rim textures live in the rim folder (%WHEELFOLDER%); env cubes and tyres come through CarTextures.
            let mut textures = CarTextures::new(&body.assets, &body.car, track);
            textures.env_dynamic = env_dynamic.clone();
            // (image, gamma-signed) from the rim folder's tex.json (fetch-constant sign bits (w0 >> 2) & 0x3F == 0x3F = gamma).
            let rim_tex: serde_json::Value = crate::files::read_json(&rim_dir.join("tex.json")).unwrap_or_default();
            let wheel_tex = |name: &str, images: &mut Assets<Image>| {
                let gamma = rim_tex[name]["w0"].as_u64().is_some_and(|w0| (w0 >> 2) & 0x3F == 0x3F);
                crate::scenery::read_dds_cached(&rim_dir.join(format!("{name}.dds"))).map(|i| (images.add(i), gamma))
            };
            let rim_base = wheel_tex("wheel", &mut images);
            let rim_base_lod0 = wheel_tex("wheel_LOD0", &mut images);
            // Every LOD1 part, blur shells included, for the game's measures; LOD0 rim parts for drawing.
            let decode = |lod: i32| -> Vec<(&FxCarSection, &crate::car::FxCarSubsection, Mesh, Vec<Vec3>)> {
                let mut v = Vec::new();
                for s in &fx.sections {
                    for sub in s.subsections.iter().filter(|x| x.lod == lod) {
                        let Some(mesh) = subsection_mesh(s, sub) else { continue };
                        let pos = crate::car_shadow::unpack_positions(&mesh, &pack_t_s(s)).unwrap_or_default();
                        v.push((s, sub, mesh, pos));
                    }
                }
                v
            };
            let lod1 = decode(1);
            let mut lod0: Vec<_> = decode(0).into_iter().filter(|p| !is_tyre(&p.1.name) && !skip_part(&p.1.name)).collect();
            // 0x82DA1CF8: Dt = 2 x max tyre radius, Dr = 2 x (max rim radius - 0.0127), Wt = tyre X extent.
            let (mut rt, mut rr, mut xlo, mut xhi) = (0.0f32, 0.0f32, f32::MAX, f32::MIN);
            for (_, sub, _, pos) in &lod1 {
                for p in pos {
                    let r = (p.y * p.y + p.z * p.z).sqrt();
                    if is_tyre(&sub.name) {
                        rt = rt.max(r);
                        xlo = xlo.min(p.x);
                        xhi = xhi.max(p.x);
                    } else if is_rim(&sub.name) {
                        rr = rr.max(r);
                    }
                }
            }
            let (dt, dr, wt) = (2.0 * rt, 2.0 * (rr - 0.0127), (xhi - xlo).max(1e-3));
            let axles: Vec<Vec4> = (0..2)
                .map(|a| {
                    let ax = &model["axles"][a];
                    let f = |k: &str| ax[k].as_f64().unwrap_or(0.0) as f32;
                    let (tr, tw, rimr) = (f("tyre_radius"), f("tyre_width"), f("rim_radius"));
                    if dt <= 0.0 || dr <= 0.0 || tr <= 0.0 {
                        return Vec4::new(1.0, 1.0, 1.0, 0.0);
                    }
                    // 0x82D921E0: x = width / Wt, y = rim diameter / Dr, z = tyre diameter / Dt, w = split radius².
                    Vec4::new(tw / wt, 2.0 * rimr / dr, 2.0 * tr / dt, ((dr + 0.3 * (dt - dr)) * 0.5).powi(2))
                })
                .collect();
            // The LOD0 rim (with the wheel emblem) when it is real geometry (at least half the LOD1 rim's triangles; some
            // rims ship a placeholder). Its pool decodes stretched to the section bounds, which include the tyre
            // (docs/WHEELS.md "LOD0 rim decode bug"), so pack_partPosition is refitted until its box matches the LOD1 rim's:
            // one radial scale k plus the box centre (fh1setup does it per axis on the vertices).
            let bbox = |idx: &[usize], v: &[(&FxCarSection, &crate::car::FxCarSubsection, Mesh, Vec<Vec3>)]| {
                let (mut lo, mut hi) = (Vec3::MAX, Vec3::MIN);
                for &i in idx {
                    for p in &v[i].3 {
                        lo = lo.min(*p);
                        hi = hi.max(*p);
                    }
                }
                (lo, hi)
            };
            let tris = |idx: &[usize], v: &[(&FxCarSection, &crate::car::FxCarSubsection, Mesh, Vec<Vec3>)]| idx.iter().map(|&i| v[i].1.indices.len() / 3).sum::<usize>();
            let ref_rim: Vec<usize> = (0..lod1.len()).filter(|&i| !is_tyre(&lod1[i].1.name) && !skip_part(&lod1[i].1.name)).collect();
            let hi_rim: Vec<usize> = (0..lod0.len()).collect();
            let use_lod0 = !hi_rim.is_empty()
                && tris(&hi_rim, &lod0) * 2 >= tris(&ref_rim, &lod1)
                && std::env::var("FH1_CARFX_WHEEL_LOD0").map_or(true, |v| v != "0");
            // The LOD0 pool is shrunk radially only (BEN_CONTINENTALGT_11: r 0.194 vs the LOD1 rim's 0.286, X -0.116..0.121
            // vs -0.113..0.116), so X gets its own scale kx like fh1setup's per-axis map. pack_partPosition's scale is one
            // scalar, so kx/k goes into the packed X of the LOD0 rim vertices. A uniform k made the LOD0 rim ~1.4x too wide:
            // it stood 3-8 cm proud of the tyre face (and as far inboard) on 73 of the 158 FH1 cars drawn with a LOD0 rim. FH1_RIM_FIX=0 = old.
            let (fit, kv) = if use_lod0 {
                let ((rlo, rhi), (lo, hi)) = (bbox(&ref_rim, &lod1), bbox(&hi_rim, &lod0));
                let span = hi - lo;
                let k = if span.y > 1e-4 && span.z > 1e-4 { 0.5 * ((rhi.y - rlo.y) / span.y + (rhi.z - rlo.z) / span.z) } else { 1.0 };
                let kx = if rim_fix() && span.x > 1e-4 { (rhi.x - rlo.x) / span.x } else { k };
                let kv = Vec3::new(kx, k, k);
                let off = (rlo + rhi) * 0.5 - (lo + hi) * 0.5 * kv;
                (off.extend(k), kv)
            } else {
                (Vec4::new(0.0, 0.0, 0.0, 1.0), Vec3::ONE)
            };
            if use_lod0 && kv.x != fit.w {
                let r = kv.x / fit.w;
                for p in &mut lod0 {
                    scale_packed_x(&mut p.2, r);
                }
            }
            if log {
                info!("fxwheel {}: rim {rim}, Dt {dt:.3} Dr {dr:.3} Wt {wt:.3}, c59 front {:?} rear {:?}, LOD0 rim {use_lod0} fit {fit:?}", body.car, axles[0], axles[1]);
            }
            let draw: Vec<(&(&FxCarSection, &crate::car::FxCarSubsection, Mesh, Vec<Vec3>), bool)> = lod1
                .iter()
                .filter(|p| !skip_part(&p.1.name) && (!use_lod0 || is_tyre(&p.1.name)))
                .map(|p| (p, false))
                .chain(lod0.iter().filter(|_| use_lod0).map(|p| (p, true)))
                .collect();
            for (k, corner) in ["LF", "RF", "LR", "RR"].iter().enumerate() {
                let scale = axles[k / 2];
                let mut out = Vec::new();
                for ((s, sub, mesh, _), refit) in &draw {
                    let Some(tech) = shading.technique(&s.name, &sub.name) else { continue };
                    let Some((_, program)) = lib.program_family(&shading.library, &tech, &shading.family, &mut shaders, &mut globals.0) else { continue };
                    let Some(mut m) = shading.material_textured(s, sub, &mut lib, &mut globals, &mut shaders, &mut textures, &mut images) else { continue };
                    set_named(&mut m, &program, "wheelScale", scale.to_array());
                    // At rest: no blur geometry/texture blend (VERIFIED: the game's (1,0,0,0) / 0 on the Viper at
                    // speed too), no tyre deformation (open; see docs/WHEELS.md).
                    set_named(&mut m, &program, "blurRimTexBlendAmounts", [1.0, 0.0, 0.0, 0.0]);
                    set_named(&mut m, &program, "blurRimGeomTexBlend", [0.0; 4]);
                    // Per axis: X's own scale kx sits in the packed X (scale_packed_x); the translation carries
                    // centre·(kv - k) so that pos·k + fit.xyz = pos·kv + off in the unpacked frame (shadow proxy too).
                    let part_fit = if *refit { fit + (Vec3::from(pack_t_s(s).center) * (kv - Vec3::splat(fit.w))).extend(0.0) } else { Vec4::new(0.0, 0.0, 0.0, 1.0) };
                    if *refit {
                        // VS: pos = xyz·w·S + T; refitted = pos·k + off.
                        let pack = crate::car::pack_part_position(s, s.pool_for(sub));
                        let t = Vec3::new(pack[0], pack[1], pack[2]) * part_fit.w + part_fit.truncate();
                        set_named(&mut m, &program, "pack_partPosition", [t.x, t.y, t.z, pack[3] * part_fit.w]);
                    }
                    let tyre = is_tyre(&sub.name);
                    let base = if *refit { rim_base_lod0.clone().or(rim_base.clone()) } else { rim_base.clone() };
                    bind_wheel_textures(&mut m, &program, base, &mut textures, &mut images);
                    // Right wheels are mirrored by their glTF node (scale x -1): flip the cull side.
                    m.flip_cull ^= k % 2 == 1;
                    out.push(Part { mesh: mesh.clone(), material: m, aabb: pack_t_s(s), wheel: Some(FxWheelPart { scale, tyre, fit: part_fit }) });
                    if log && k == 0 {
                        info!("fxwheel {} / {} (LOD{}) -> {tech}", s.name, sub.name, sub.lod);
                    }
                }
                pending.0.push((format!("wheel_{corner}"), out));
            }
        }

        // ---- rotors + calipers (the car's own shading and atlas) ----
        let dir = cars.join(&body.car).join("fx");
        let metallic = model["paint"]["metallic"].as_bool().unwrap_or(true);
        let settings = ShaderSettings::chain(&[
            read(shared.join("Normal.xml")).as_deref(),
            read(shared.join(if metallic { "Metallic.xml" } else { "Matte.xml" })).as_deref(),
            read(dir.join("ShaderSettings.xml")).as_deref(),
        ]);
        if let Some(mut shading) = CarShading::new("shaders_v16", &lib_bytes, settings, &mut lib) {
            widen_family(&mut shading);
            let mut textures = CarTextures::new(&body.assets, &body.car, track);
            textures.env_dynamic = env_dynamic.clone();
            for kind in ["rotor", "caliper"] {
                for corner in ["LF", "RF", "LR", "RR"] {
                    // Imported cars are addressed by path (`../imported/<game>/cars/<media>`): file names use the media name.
                    let stem = std::path::Path::new(&body.car).file_name().and_then(|s| s.to_str()).unwrap_or(&body.car);
                    let Some(fx) = crate::files::read(&dir.join(format!("{stem}_{kind}{corner}_LOD0.fxcar"))).and_then(|b| parse_fxcar(&b)) else { continue };
                    shading.sh_compression = fx.sh_compression.unwrap_or(SH_COMPRESSION_DEFAULT);
                    let mut out = Vec::new();
                    for s in &fx.sections {
                        let lod = s.subsections.iter().map(|x| x.lod).min().unwrap_or(0);
                        for sub in s.subsections.iter().filter(|x| x.lod == lod) {
                            // Placeholder cubes (`<section>_LOD<n>`) are skipped like fh1setup does.
                            if sub.name.contains("_LOD") {
                                continue;
                            }
                            let (Some(mesh), Some(m)) = (subsection_mesh(s, sub), shading.material_textured(s, sub, &mut lib, &mut globals, &mut shaders, &mut textures, &mut images)) else { continue };
                            out.push(Part { mesh, material: m, aabb: pack_t_s(s), wheel: Some(FxWheelPart::RIGID) });
                        }
                    }
                    pending.0.push((format!("{kind}_{corner}"), out));
                }
            }
        }
        commands.entity(e).insert(pending);
    }
}

/// Make the per-wheel constants material registers in this shading's programs.
fn widen_family(shading: &mut CarShading) {
    if let Family::Car { material_names } = &shading.family {
        let mut names: HashSet<String> = (**material_names).clone();
        names.extend(WHEEL_CONSTANTS.iter().map(|s| s.to_string()));
        shading.family = Family::Car { material_names: Arc::new(names) };
    }
}

/// rim_V2 baseSampler = the rim folder's `wheel` texture (textures.xml %WHEELFOLDER%, INFERRED for the non-SuperLOD
/// name); tire_V2 blurTireSampler1-4 = Shared `tireA0`..`tireA3` (engine-bound; INFERRED: the static tyre is A0 and
/// A1-3 its blur steps).
fn bind_wheel_textures(m: &mut FxCarMaterial, program: &Program, rim_base: Option<(Handle<Image>, bool)>, textures: &mut CarTextures, images: &mut Assets<Image>) {
    for (tf, dim) in &program.textures {
        if *dim == 3 {
            continue;
        }
        let Some(name) = program.samplers.iter().find(|(r, _)| r == tf).map(|(_, n)| n.as_str()) else { continue };
        let tex = match name {
            "baseSampler" => rim_base.clone(),
            "blurTireSampler" => textures.get("tireA0", false, images).map(|(h, g, _)| (h, g)),
            "blurTireSampler2" => textures.get("tireA1", false, images).map(|(h, g, _)| (h, g)),
            "blurTireSampler3" => textures.get("tireA2", false, images).map(|(h, g, _)| (h, g)),
            "blurTireSampler4" => textures.get("tireA3", false, images).map(|(h, g, _)| (h, g)),
            _ => None,
        };
        if let (Some((h, gamma)), Some(slot)) = (tex, m.slot_mut(*tf)) {
            *slot = Some(h);
            if gamma {
                m.consts.gamma.x |= 1 << tf;
            } else {
                m.consts.gamma.x &= !(1 << tf);
            }
        }
    }
}

/// Once the glTF scene has spawned the nodes: put the parts under them and hide the glTF meshes they replace.
#[allow(clippy::too_many_arguments)]
fn attach_fx_wheels(
    mut commands: Commands,
    mut cars: Query<(Entity, &mut FxWheelsPending)>,
    children: Query<&Children>,
    names: Query<&Name>,
    has_mesh: Query<(), With<Mesh3d>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxCarMaterial>>,
) {
    for (car, mut pending) in &mut cars {
        if pending.0.is_empty() {
            commands.entity(car).remove::<FxWheelsPending>();
            continue;
        }
        let nodes: Vec<(Entity, String)> = children.iter_descendants(car).filter_map(|d| names.get(d).ok().map(|n| (d, n.as_str().to_string()))).collect();
        pending.0.retain_mut(|(node_name, parts)| {
            let Some(&(node, _)) = nodes.iter().find(|(_, n)| n == node_name) else { return true };
            if parts.is_empty() {
                return false;
            }
            // The glTF meshes: wheel nodes hold them as children; rotor/caliper nodes hold the mesh themselves.
            if has_mesh.contains(node) {
                commands.entity(node).remove::<Mesh3d>();
            }
            for c in children.get(node).map(|c| c.iter().collect::<Vec<_>>()).unwrap_or_default() {
                commands.entity(c).insert((Visibility::Hidden, ReplacedGltfWheel));
            }
            for p in parts.drain(..) {
                let mut ec = commands.spawn((Mesh3d(meshes.add(p.mesh)), MeshMaterial3d(materials.add(p.material)), p.aabb, Transform::default(), ChildOf(node)));
                if let Some(w) = p.wheel {
                    ec.insert(w);
                }
            }
            false
        });
    }
}

/// A glTF wheel/brake mesh node hidden in favour of the game-shaded parts.
#[derive(Component)]
struct ReplacedGltfWheel;

/// FH1_WHEELS_AB=1: in one run, cycle every 5 s through fx (game-shaded wheels + their CSM
/// proxies), fx_no_csm (no wheel proxies) and gltf (the glTF wheels), and log each mode's mean frame time per cycle and
/// the running means (first cycle dropped). Both modes see the same machine load. The wheels never reach the env cube
/// (reflect.rs skips the anchored car and meshes under 2 m).
#[allow(clippy::type_complexity)]
fn wheels_ab(
    time: Res<Time<Real>>,
    mut state: Local<(f32, usize, u32, f32, u32, [(f64, u32); 3])>,
    mut fx: Query<(&mut Visibility, Option<&crate::car_shadow::CarCasterProxy>), (With<FxWheelPart>, Without<ReplacedGltfWheel>)>,
    mut gltf: Query<&mut Visibility, (With<ReplacedGltfWheel>, Without<FxWheelPart>)>,
    mut proxies: Query<&mut Visibility, (Without<FxWheelPart>, Without<ReplacedGltfWheel>)>,
) {
    const MODES: [&str; 3] = ["fx", "fx_no_csm", "gltf"];
    let (t, mode, frames, sum, cycle, totals) = &mut *state;
    let dt = time.delta_secs();
    *t += dt;
    *frames += 1;
    *sum += dt;
    if *t < 5.0 {
        return;
    }
    let mean = *sum / *frames as f32 * 1000.0;
    // Cycle 0 (loading, shader compiles) is dropped.
    if *cycle > 0 {
        totals[*mode].0 += mean as f64;
        totals[*mode].1 += 1;
    }
    let means: Vec<String> = (0..3).map(|k| format!("{} {:.2}", MODES[k], if totals[k].1 > 0 { totals[k].0 / totals[k].1 as f64 } else { 0.0 })).collect();
    info!("wheels AB cycle {} mode {} mean {mean:.2} ms | running: {}", *cycle, MODES[*mode], means.join(", "));
    *mode = (*mode + 1) % 3;
    if *mode == 0 {
        *cycle += 1;
    }
    *t = 0.0;
    *frames = 0;
    *sum = 0.0;
    let fx_on = *mode != 2;
    for (mut v, proxy) in &mut fx {
        *v = if fx_on { Visibility::Inherited } else { Visibility::Hidden };
        if let Some(p) = proxy {
            if let Ok(mut pv) = proxies.get_mut(p.0) {
                *pv = if *mode == 0 { Visibility::Inherited } else { Visibility::Hidden };
            }
        }
    }
    for mut v in &mut gltf {
        *v = if fx_on { Visibility::Hidden } else { Visibility::Inherited };
    }
}
