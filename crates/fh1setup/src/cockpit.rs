//! Car interiors (owner: fh1-rewrite-30; docs/CAR_INTERIOR.md).
//!
//! Two different interiors exist per car:
//! - **Seen from outside**: the main carbin's LOD1+ `interior` / `seatL` / `seatR` / `steering_wheel` sections
//!   (`_lod0.carbin` has no interior sections). Their material is `interior`, whose technique samples
//!   `baseSampler` = the BODY atlas (`nodamage.xds`), not `interior_LOD0` (Shared `textures.xml` loads
//!   `Interior_LOD0.xds` only with loadFlags `Cockpit & !SuperLOD`). Its pixel shader (shaders_v16 ps 286)
//!   takes `base.rgb * base.a` (premultipliedAlpha off), then lighting x FinalScale 0.4 (Normal.xml `<interior>`).
//!   [`interior_atlas`] bakes that: `interior_ext.png` = nodamage rgb x alpha, opaque.
//! - **Cockpit** (`<car>_cockpit.carbin`, LOD0 only): seats, wheel, dash, gauges, needles, the A-pillar shell.
//!   `cockpit_*` techniques (ps 326/330/334...) take `CockpitInteriorHiSampler.rgb` (= `interior_LOD0`) at
//!   uv0 x uv1CompressionFactors (the subsection's first UV transform), no alpha multiply. [`export`] writes
//!   `cockpit.gltf` + `cockpit.bin` + `cockpit.png`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use fh1_formats::carbin::{self, Section, Subsection};
use fh1_formats::xds;
use fh1_formats::zip::Archive;

use crate::model::Paint;

/// Section names that get `cockpit_` techniques (fh1-render `car.rs` INTERIOR_WORDS, from default.xex).
const INTERIOR_WORDS: [&str; 22] = [
    "interior", "seat", "steering_wheel", "speed", "fuel", "boost", "oiltemp", "oilpressure", "watertemp", "volt", "gaugea",
    "gaugeb", "gaugec", "gauged", "gaugee", "gaugef", "power", "doorcard", "tach", "boot", "gauge", "door_card",
];

/// The ShaderSettings chain for a car folder written by the cars group: Shared `Normal.xml`, then the car's own
/// `ShaderSettings.xml` (later files override earlier ones; docs/SHADERS.md "Parameters").
fn settings_chain(car_dir: &Path) -> Vec<String> {
    let shared = car_dir.parent().map(|c| c.join("shared/ShaderSettings/Normal.xml"));
    [shared, Some(car_dir.join("fx/ShaderSettings.xml"))].into_iter().flatten().filter_map(|p| std::fs::read_to_string(p).ok()).collect()
}

/// A technique's parameter from the chain: the first `<Name value=".."/>` or `<Name r=".." g=".." b=".."/>` in
/// the `<technique>` block whose name passes `pick`. Later files win.
fn setting(chain: &[String], technique: &str, pick: impl Fn(&str) -> bool) -> Option<[f32; 3]> {
    let attr = |line: &str, k: &str| -> Option<f32> {
        let at = line.find(&format!(" {k}=\""))? + k.len() + 3;
        line[at..].split('"').next()?.trim().parse().ok()
    };
    let mut out = None;
    for text in chain {
        let open = format!("<{technique}>");
        let Some(start) = text.find(&open) else { continue };
        let body = &text[start + open.len()..];
        let body = &body[..body.find(&format!("</{technique}>")).unwrap_or(body.len())];
        for line in body.lines().map(str::trim) {
            let name = line.trim_start_matches('<').split([' ', '/', '>']).next().unwrap_or("");
            if !pick(name) {
                continue;
            }
            if let Some(v) = attr(line, "value") {
                out = Some([v; 3]);
            } else if let (Some(r), Some(g), Some(b)) = (attr(line, "r"), attr(line, "g"), attr(line, "b")) {
                out = Some([r, g, b]);
            }
            break;
        }
    }
    out
}

/// FinalScale of the exterior model's `interior` technique (Normal.xml: 0.4), from the car's settings chain.
pub fn interior_final_scale(car_dir: &Path) -> [f32; 3] {
    setting(&settings_chain(car_dir), "interior", |n| n == "FinalScale").unwrap_or([0.4; 3])
}

struct Rgba {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Rgba {
    fn decode(xds_file: &[u8]) -> Result<Self> {
        let (_, img) = xds::decode_base(xds_file)?;
        Ok(Self { w: img.width as usize, h: img.height as usize, px: xds::to_rgba8(&img)? })
    }

    /// Linear RGB of the texel at (u, v), point sampled.
    fn texel(&self, u: f32, v: f32) -> [f32; 3] {
        let x = ((u.rem_euclid(1.0) * self.w as f32) as usize).min(self.w - 1);
        let y = ((v.rem_euclid(1.0) * self.h as f32) as usize).min(self.h - 1);
        let p = &self.px[(y * self.w + x) * 4..][..3];
        [0, 1, 2].map(|k| srgb_to_linear(p[k]))
    }
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// `interior_ext.png` for the exterior model's `interior` material: `nodamage.xds` (the LOD1 body atlas) with
/// rgb x alpha, opaque. Returns the file name.
pub fn interior_atlas(nodamage_xds: &[u8], out: &Path) -> Result<String> {
    let mut t = Rgba::decode(nodamage_xds)?;
    for p in t.px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
        p[3] = 255;
    }
    let name = "interior_ext.png";
    write_png(&out.join(name), t.w as u32, t.h as u32, &t.px)?;
    Ok(name.into())
}

/// The cars-group hook (after `model::export` and `carfx::export_car`, which writes the car's ShaderSettings):
/// `interior_ext.png`, the exterior model's `interior` material pointed at it with the car's FinalScale, and
/// the cockpit model. Returns the cockpit info (`None` without a cockpit carbin).
pub fn build_car(zip: &Path, media: &str, paint: &Paint, out: &Path) -> Result<Option<Value>> {
    let gltf_path = out.join("model.gltf");
    if gltf_path.exists() {
        let mut ar = Archive::open(zip)?;
        if let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("nodamage.xds")).cloned() {
            let png = interior_atlas(&ar.read(&e)?, out)?;
            let scale = interior_final_scale(out);
            let mut g: Value = serde_json::from_slice(&std::fs::read(&gltf_path)?)?;
            // Idempotent: reuse the image / texture entries if an earlier run added them.
            let image = {
                let images = g["images"].as_array_mut().context("model.gltf images")?;
                match images.iter().position(|i| i["uri"] == png.as_str()) {
                    Some(i) => i,
                    None => {
                        images.push(json!({"uri": png}));
                        images.len() - 1
                    }
                }
            };
            let texture = {
                let textures = g["textures"].as_array_mut().context("model.gltf textures")?;
                match textures.iter().position(|t| t["source"] == image) {
                    Some(t) => t,
                    None => {
                        textures.push(json!({"source": image, "sampler": 0}));
                        textures.len() - 1
                    }
                }
            };
            for m in g["materials"].as_array_mut().into_iter().flatten() {
                if m["name"] == "interior" {
                    let pbr = &mut m["pbrMetallicRoughness"];
                    pbr["baseColorTexture"] = json!({"index": texture});
                    pbr["baseColorFactor"] = json!([scale[0], scale[1], scale[2], 1.0]);
                }
            }
            std::fs::write(&gltf_path, serde_json::to_vec(&g)?)?;
        }
    }
    export(zip, media, paint, out)
}

/// Writes `cockpit.gltf` / `cockpit.bin` / `cockpit.png` from `<media>_cockpit.carbin`. `Ok(None)` when the car
/// has no cockpit. Same mesh space as `model.gltf` (origin bottom-centre of the body, front -Z).
pub fn export(zip: &Path, media: &str, paint: &Paint, out: &Path) -> Result<Option<Value>> {
    let mut ar = Archive::open(zip)?;
    let mut read = |name: &str| -> Result<Option<Vec<u8>>> {
        match ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned() {
            Some(e) => Ok(Some(ar.read(&e)?)),
            None => Ok(None),
        }
    };
    let Some(d) = read(&format!("{media}_cockpit.carbin"))? else { return Ok(None) };
    let c = carbin::parse(&d).context("cockpit carbin")?;
    // Six traffic-only cars (AUD_A4Avant_04, BUS_TOUR_12, TOY_Camry_07, ...) ship a cockpit carbin but no atlas.
    let Some(atlas) = read("interior_LOD0.xds")? else { return Ok(None) };
    let atlas = Rgba::decode(&atlas)?;
    write_png(&out.join("cockpit.png"), atlas.w as u32, atlas.h as u32, &atlas.px)?;
    // The A-pillar shell uses body materials on the exterior atlas (written by model.rs).
    let exterior = out.join("exterior.png").exists().then_some("exterior.png");

    let chain = settings_chain(out);
    let mut g = Gltf { paint_rgb: paint.rgb, metallic_paint: paint.metallic, ..Default::default() };
    let mut children = Vec::new();
    let mut skipped = Vec::new();
    for s in &c.sections {
        let lname = s.name.to_lowercase();
        // Race cage is an upgrade; needles exist at min/mid/max, keep the rest position.
        if lname.starts_with("cage") || lname.ends_with("race") || lname.ends_with("_mid") || lname.ends_with("_max") {
            skipped.push(s.name.clone());
            continue;
        }
        let interior = INTERIOR_WORDS.iter().any(|w| lname.contains(w));
        let subs: Vec<&Subsection> = s.subsections.iter().filter(|x| x.lod == 0).collect();
        if let Some(mesh) = g.mesh(&s.name, s, &subs, interior, &atlas, exterior, &chain) {
            children.push(g.node(json!({"name": s.name, "mesh": mesh})));
        }
    }
    // model.rs (cars-13) translates model.gltf's root by `mesh_offset` (model.json; physics frame is
    // wheelbase-centred, carbin meshes are body-bottom-centred). Bake the same offset so both line up.
    let offset = std::fs::read(out.join("model.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map(|v| v["mesh_offset"].clone()).filter(|v| v.is_array());
    let mut root = json!({"name": format!("{media}_cockpit"), "children": children});
    if let Some(t) = offset {
        root["translation"] = t;
    }
    let root = g.node(root);
    let info = json!({"triangles": g.triangles, "materials": g.materials.len(), "winding_flipped": g.flipped, "skipped_sections": skipped});
    g.write(out, root)?;
    Ok(Some(info))
}

struct Mat {
    color: [f32; 4],
    metallic: f32,
    roughness: f32,
    /// Image file, or None for a constant colour.
    image: Option<&'static str>,
}

/// Material for one subsection. `region`: the subsection's UV transform maps onto a real atlas region (else
/// it points at a single texel, which is the constant colour the game samples: swatches, glass tint).
fn material(section_interior: bool, name: &str, xf: [f32; 4], atlas: &Rgba, exterior: Option<&'static str>, paint: (u32, bool)) -> Mat {
    let n = name.to_lowercase();
    let has = |k: &str| n.contains(k);
    let region = xf[1].abs() > 1e-5 && xf[3].abs() > 1e-5;
    let m = |color: [f32; 3], a: f32, metallic, roughness, image| Mat { color: [color[0], color[1], color[2], a], metallic, roughness, image };
    if has("mirror") {
        // Mirror surfaces (render targets in the game).
        return m([0.6; 3], 1.0, 1.0, 0.05, None);
    }
    if !section_interior {
        // Body materials seen from inside (A-pillar, roof shell): same rules as the exterior model.
        if has("glass") || has("window") {
            return m([0.08, 0.1, 0.11], 0.45, 0.0, 0.05, None);
        }
        if n == "body" || n.starts_with("paint") {
            let [_, r, g, b] = paint.0.to_be_bytes();
            return m([r, g, b].map(srgb_to_linear), 1.0, if paint.1 { 0.6 } else { 0.0 }, 0.3, exterior);
        }
        if has("black") || has("rubber") || has("frame") {
            return m([0.03; 3], 1.0, 0.0, 0.7, None);
        }
        if has("chrome") {
            return m([0.95; 3], 1.0, 1.0, 0.1, exterior);
        }
        return m([1.0; 3], 1.0, 0.0, 0.5, exterior);
    }
    let metal = has("metal") || has("chrome");
    let (metallic, roughness) = if metal { (0.9, 0.3) } else if has("leather") { (0.0, 0.6) } else if has("glass") || has("reflector") { (0.0, 0.1) } else { (0.0, 0.75) };
    if !region {
        let c = atlas.texel(xf[0], xf[2]);
        // Metal techniques get their look from the environment map; keep a bare metal base visible.
        let c = if metal { c.map(|v| v.max(0.5)) } else { c };
        let a = if has("glass") { 0.35 } else { 1.0 };
        return m(c, a, metallic, roughness, None);
    }
    if has("carbon") && xf[1].abs() > 2.0 {
        // Tiling UVs for the shared carbon weave; flat dark carbon until that texture is bound.
        return m([0.05; 3], 1.0, 0.2, 0.3, None);
    }
    // z_gauge_emissive_alpha: alpha-blended overlay (needle glows, warning lamps).
    let a = if has("alpha") { 0.999 } else { 1.0 };
    m([1.0; 3], a, metallic, roughness, Some("cockpit.png"))
}

#[derive(Default)]
struct Gltf {
    bin: Vec<u8>,
    buffer_views: Vec<Value>,
    accessors: Vec<Value>,
    meshes: Vec<Value>,
    nodes: Vec<Value>,
    materials: Vec<Value>,
    material_ids: HashMap<String, usize>,
    images: Vec<Value>,
    image_ids: HashMap<String, usize>,
    flipped: bool,
    winding_decided: bool,
    triangles: usize,
    paint_rgb: u32,
    metallic_paint: bool,
}

impl Gltf {
    fn node(&mut self, n: Value) -> usize {
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

    fn material(&mut self, key: String, mat: Mat, name: &str) -> usize {
        if let Some(&id) = self.material_ids.get(&key) {
            return id;
        }
        let mut pbr = json!({"baseColorFactor": mat.color, "metallicFactor": mat.metallic, "roughnessFactor": mat.roughness});
        if let Some(png) = mat.image {
            let img = *self.image_ids.entry(png.to_string()).or_insert_with(|| {
                self.images.push(json!({"uri": png}));
                self.images.len() - 1
            });
            pbr["baseColorTexture"] = json!({"index": img});
        }
        let mut m = json!({"name": name, "pbrMetallicRoughness": pbr, "doubleSided": false});
        if mat.color[3] < 1.0 {
            m["alphaMode"] = json!("BLEND");
            m["doubleSided"] = json!(true);
        }
        self.materials.push(m);
        self.material_ids.insert(key, self.materials.len() - 1);
        self.materials.len() - 1
    }

    fn mesh(&mut self, name: &str, s: &Section, subs: &[&Subsection], interior: bool, atlas: &Rgba, exterior: Option<&'static str>, chain: &[String]) -> Option<usize> {
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
                .filter(|&&i| (i as usize) < pool.len())
                .map(|&i| {
                    *remap.entry(i).or_insert_with(|| {
                        verts.push(pool[i as usize]);
                        verts.len() as u32 - 1
                    })
                })
                .collect();
            idx.truncate(idx.len() / 3 * 3);
            let pos: Vec<[f32; 3]> = verts.iter().map(|v| [0, 1, 2].map(|k| v.position[k] + s.offset[k])).collect();
            // Winding: decided once per file against the stored normals, as model.rs does.
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
            let xf = [sub.uv_transform[0], sub.uv_transform[1], sub.uv_transform[2], sub.uv_transform[3]];
            let uvs: Vec<[f32; 2]> = verts.iter().map(|v| [v.uv0[0] * xf[1] + xf[0], v.uv0[1] * xf[3] + xf[2]]).collect();
            let mut mat = material(interior, &sub.name, xf, atlas, exterior, (self.paint_rgb, self.metallic_paint));
            // Metals (cockpit_metal PaintScale 0.002) are almost all specular + cube map in the game; StandardMaterial
            // gets that from metallic with the texture as F0, so their diffuse scale is not applied.
            if interior && mat.metallic < 0.5 {
                // cockpit_* pixel shaders multiply the CockpitInteriorHiSampler colour by the technique's
                // *PaintScale (ps 326 CockpitPlasticPaintScale c124, ps 330 PaintScale c157).
                let scale = setting(chain, &format!("cockpit_{}", sub.name), |n| n.ends_with("PaintScale")).unwrap_or([1.0; 3]);
                for k in 0..3 {
                    mat.color[k] *= scale[k];
                }
            }
            let key = format!("{interior}|{}|{:?}|{:?}", sub.name, mat.image, mat.color);
            let mat_name = if interior { format!("cockpit_{}", sub.name) } else { sub.name.clone() };
            let mat_id = self.material(key, mat, &mat_name);

            let mut lo = [f32::MAX; 3];
            let mut hi = [f32::MIN; 3];
            for p in &pos {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
            let v = self.view(f32_bytes(pos.as_flattened()), 34962);
            let a_pos = self.accessor(v, 5126, pos.len(), "VEC3", Some((lo, hi)));
            let v = self.view(f32_bytes(normals.as_flattened()), 34962);
            let a_nrm = self.accessor(v, 5126, normals.len(), "VEC3", None);
            let v = self.view(f32_bytes(uvs.as_flattened()), 34962);
            let a_uv = self.accessor(v, 5126, uvs.len(), "VEC2", None);
            let ib: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
            let v = self.view(&ib, 34963);
            let a_idx = self.accessor(v, 5125, idx.len(), "SCALAR", None);
            self.triangles += idx.len() / 3;
            prims.push(json!({"attributes": {"POSITION": a_pos, "NORMAL": a_nrm, "TEXCOORD_0": a_uv}, "indices": a_idx, "material": mat_id}));
        }
        if prims.is_empty() {
            return None;
        }
        self.meshes.push(json!({"name": name, "primitives": prims}));
        Some(self.meshes.len() - 1)
    }

    fn write(self, out: &Path, root: usize) -> Result<()> {
        std::fs::write(out.join("cockpit.bin"), &self.bin)?;
        let samplers = json!([{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}]);
        let textures: Vec<Value> = (0..self.images.len()).map(|i| json!({"source": i, "sampler": 0})).collect();
        let doc = json!({
            "asset": {"version": "2.0", "generator": "fh1setup cockpit"},
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
            "buffers": [{"uri": "cockpit.bin", "byteLength": self.bin.len()}],
        });
        std::fs::write(out.join("cockpit.gltf"), serde_json::to_vec(&doc)?)?;
        Ok(())
    }
}

fn f32_bytes(v: &[f32]) -> &[u8] {
    // f32 has no padding; little-endian hosts only (Windows x64), as model.rs.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

fn tri_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]]
}

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

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}
