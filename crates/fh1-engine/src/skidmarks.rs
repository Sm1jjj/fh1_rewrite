//! Driving effects: tyre skid marks and car dirt build-up (docs/EFFECTS.md "Skid marks", "Car dirt").
//!
//! Skid marks: FH1 keeps one ring buffer of mark quads for all cars (
//! 825A8A70: 9,000 vertices of float3 position, float3 uv, D3DCOLOR; 16,000 indices; decl "SkidMarksDecl"; texture atlas
//! `media/tracks/treadmark.xds`, 512x128 DXT5 = four 128 px columns). Per surface, surfaceTypes.xml `SkidData` gives the
//! mark's colour, alpha, atlas column (TextureIndex) and a MinIntensity (0.4 on dirt / grass / gravel: tracks are left even
//! without slip). Intensity from the combined normalised slip over PhysicsSettings SkidGraphicsFricCircleMin..Max
//! (0.95..2). Here: 16 chunk meshes x 144 quads (one draw each, only the chunk being written is re-uploaded), StandardMaterial
//! unlit + alpha blend, a 2 cm lift plus depth bias.
//!
//! Car dirt: GlobalCarAttribs.xml `<Dirt DistanceForFullDirtInMiles="10" MinSpeedForDirtAccumulatorInMPH="20"/>` (loader
//! 82D91088; MaxDirtApplied is not in the file): the dirt level grows with the distance driven above 20 mph, full after
//! 10 miles, whatever the surface. It is the car shaders' alphaDirtAmount.y (the game read 0.00875 at the festival, i.e.
//! 0.0875 miles in). Fed through the shared car constant bank `FxCarGlobals`.
//!
//! FH1_SKIDMARKS=0 / FH1_CAR_DIRT=0 turn either off; FH1_SKID_GAIN scales the mark alpha (default 2, see EFFECTS.md).

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::track::Track;
use crate::{Car, Garage};

const CHUNKS: usize = 16;
const SEGS_PER_CHUNK: usize = 144;
/// Minimum distance between mark edges (m).
const SEG_LEN: f32 = 0.25;
/// A jump bigger than this (teleport, reset) starts a new strip.
const MAX_SEG: f32 = 3.0;
/// Lift off the ground (m), plus the material's depth bias.
const LIFT: f32 = 0.02;
/// PhysicsSettings SkidGraphicsFricCircleMin / Max: combined normalised slip where marks start / reach full intensity.
const FRIC_CIRCLE: [f32; 2] = [0.95, 2.0];
/// GlobalCarAttribs Dirt.
const DIRT_FULL_MILES: f32 = 10.0;
const DIRT_MIN_SPEED: f32 = 20.0 * 0.447_04;
/// MaxDirtApplied: not in GlobalCarAttribs.xml; its constructor default is not read yet (taken as 1).
const DIRT_MAX: f32 = 1.0;

fn flag(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| v != "0")
}

pub struct DrivingEffectsPlugin;

impl Plugin for DrivingEffectsPlugin {
    fn build(&self, app: &mut App) {
        if flag("FH1_SKIDMARKS") {
            app.add_systems(Startup, setup_skidmarks).add_systems(Update, lay_skidmarks.after(crate::sync_visuals));
        }
        if flag("FH1_CAR_DIRT") {
            app.init_resource::<CarDirt>().add_systems(Update, accumulate_dirt);
        }
    }
}

/// Mark style for one surface (surfaceTypes.xml SkidData).
#[derive(Clone, Copy)]
struct SkidStyle {
    color: [f32; 3],
    alpha: f32,
    column: u32,
    min_intensity: f32,
}

/// Asphalt: black, alpha 200, column 0.
const ASPHALT: SkidStyle = SkidStyle { color: [0.0; 3], alpha: 200.0 / 255.0, column: 0, min_intensity: 0.0 };

#[derive(Clone, Copy)]
struct Edge {
    left: Vec3,
    right: Vec3,
    centre: Vec3,
    v: f32,
    alpha: f32,
    style_column: u32,
}

struct Chunk {
    mesh: Handle<Mesh>,
    positions: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    colors: Vec<[f32; 4]>,
    dirty: bool,
}

#[derive(Resource)]
struct SkidMarks {
    chunks: Vec<Chunk>,
    /// Next quad to write (ring over CHUNKS x SEGS_PER_CHUNK).
    cursor: usize,
    last: [Option<Edge>; 4],
    styles: Vec<Option<SkidStyle>>,
    styles_for: String,
    gain: f32,
}

fn setup_skidmarks(mut commands: Commands, garage: Res<Garage>, mut meshes: ResMut<Assets<Mesh>>, mut images: ResMut<Assets<Image>>, mut materials: ResMut<Assets<StandardMaterial>>) {
    let atlas = images.add(load_atlas(&garage.assets.join("tracks/treadmark.xds")));
    let material = materials.add(StandardMaterial {
        base_color_texture: Some(atlas),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        cull_mode: None,
        double_sided: true,
        depth_bias: 50.0,
        fog_enabled: false,
        ..default()
    });
    let n = SEGS_PER_CHUNK * 4;
    let mut indices = Vec::with_capacity(SEGS_PER_CHUNK * 6);
    for q in 0..SEGS_PER_CHUNK as u16 {
        let b = q * 4;
        indices.extend_from_slice(&[b, b + 1, b + 2, b + 1, b + 3, b + 2]);
    }
    let mut chunks = Vec::with_capacity(CHUNKS);
    for _ in 0..CHUNKS {
        let (positions, uvs, colors) = (vec![[0.0; 3]; n], vec![[0.0; 2]; n], vec![[0.0; 4]; n]);
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD);
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions.clone());
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; n]);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs.clone());
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors.clone());
        mesh.insert_indices(Indices::U16(indices.clone()));
        let handle = meshes.add(mesh);
        commands.spawn((
            Mesh3d(handle.clone()),
            MeshMaterial3d(material.clone()),
            Transform::default(),
            NoFrustumCulling,
            NotShadowCaster,
            NotShadowReceiver,
            Name::new("skid marks"),
        ));
        chunks.push(Chunk { mesh: handle, positions, uvs, colors, dirty: false });
    }
    let gain = std::env::var("FH1_SKID_GAIN").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
    commands.insert_resource(SkidMarks { chunks, cursor: 0, last: [None; 4], styles: Vec::new(), styles_for: String::new(), gain });
}

/// treadmark.xds (copied by fh1setup's tracks group), else a stand-in with the same four column profiles.
fn load_atlas(path: &std::path::Path) -> Image {
    let decoded = std::fs::read(path).ok().and_then(|b| {
        let (_, img) = fh1_formats::xds::decode_base(&b).ok()?;
        let rgba = fh1_formats::xds::to_rgba8(&img).ok()?;
        Some((img.width, img.height, rgba))
    });
    let (w, h, rgba) = decoded.unwrap_or_else(|| {
        info!("skid marks: {} missing, using a stand-in atlas", path.display());
        stand_in_atlas()
    });
    let mut image = Image::new(Extent3d { width: w, height: h, depth_or_array_layers: 1 }, TextureDimension::D2, rgba, TextureFormat::Rgba8UnormSrgb, RenderAssetUsages::RENDER_WORLD);
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    });
    image
}

/// Columns 0 / 2: grey (115) tread lines at alpha ~50; 1 / 3: dark brown (16, 12, 8) with a soft-edged alpha ~124 (read
/// from the real atlas).
fn stand_in_atlas() -> (u32, u32, Vec<u8>) {
    let (w, h) = (512u32, 128u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let (col, u) = (x / 128, (x % 128) as f32 / 127.0);
            let edge = (1.0 - (2.0 * u - 1.0).abs()).min(0.25) / 0.25;
            let (rgb, a) = if col % 2 == 0 {
                let lines = if (x / 9 + y / 32) % 2 == 0 { 1.0 } else { 0.6 };
                ([115u8; 3], 55.0 * edge * lines)
            } else {
                ([16, 12, 8], 124.0 * edge)
            };
            let i = ((y * w + x) * 4) as usize;
            px[i..i + 3].copy_from_slice(&rgb);
            px[i + 3] = a as u8;
        }
    }
    (w, h, px)
}

fn surface_style(track: &Track, id: u8) -> Option<SkidStyle> {
    let Some(world) = track.world.as_ref() else { return Some(ASPHALT) };
    let s = world.world.surface(id)?;
    let p = |k: &str| s.param(&format!("SkidData/{k}"));
    let alpha = p("ColorAlpha")? / 255.0;
    Some(SkidStyle {
        color: [p("ColorRed").unwrap_or(0.0) / 255.0, p("ColorGreen").unwrap_or(0.0) / 255.0, p("ColorBlue").unwrap_or(0.0) / 255.0],
        alpha,
        column: p("TextureIndex").unwrap_or(0.0) as u32,
        min_intensity: p("MinIntensity").unwrap_or(0.0),
    })
}

fn lay_skidmarks(mut marks: ResMut<SkidMarks>, track: Res<Track>, cars: Query<&Car>, mut meshes: ResMut<Assets<Mesh>>) {
    let marks = &mut *marks;
    if marks.styles_for != track.id {
        marks.styles = (0..=255u8).map(|id| surface_style(&track, id)).collect();
        marks.styles_for = track.id.clone();
        marks.last = [None; 4];
    }
    let Ok(car) = cars.single() else { return };
    let v = &car.0;
    let speed = v.speed();
    for i in 0..4 {
        let w = &v.wheels[i];
        let style = if w.grounded { marks.styles[w.surface as usize] } else { None };
        let Some(style) = style.filter(|s| s.alpha > 0.0 && speed > 0.5) else {
            marks.last[i] = None;
            continue;
        };
        let rho = (w.norm_slip * w.norm_slip + w.norm_slip_angle * w.norm_slip_angle).sqrt();
        let slip = ((rho - FRIC_CIRCLE[0]) / (FRIC_CIRCLE[1] - FRIC_CIRCLE[0])).clamp(0.0, 1.0);
        let intensity = if speed > 1.0 { slip.max(style.min_intensity) } else { slip };
        let alpha = (style.alpha * intensity * marks.gain).min(1.0);
        let axle = i / 2;
        let normal = if w.normal.length_squared() > 0.5 { w.normal } else { v.rotation * Vec3::Y };
        let hub = v.position + v.rotation * (Vec3::from(v.data.hubs[i]) - v.cg_model + Vec3::Y * v.wheel_drop(i));
        let centre = hub - normal * v.data.tyre_radius[axle] + normal * LIFT;
        // Columns 0..1 = TextureIndex; the right-hand wheels use the second set (2..3) for variety (INFERRED pairing).
        let column = (style.column.min(1) + if i % 2 == 1 { 2 } else { 0 }).min(3);
        let Some(last) = marks.last[i] else {
            if alpha > 0.0 {
                marks.last[i] = Some(edge(centre, v.velocity, normal, v.data.tyre.width_m[axle], 0.0, alpha, column));
            }
            continue;
        };
        let d = centre.distance(last.centre);
        if d > MAX_SEG || alpha <= 0.0 && last.alpha <= 0.0 {
            marks.last[i] = (alpha > 0.0).then(|| edge(centre, v.velocity, normal, v.data.tyre.width_m[axle], 0.0, alpha, column));
            continue;
        }
        if d < SEG_LEN {
            continue;
        }
        let width = v.data.tyre.width_m[axle].max(0.1);
        let next = edge(centre, centre - last.centre, normal, width, last.v + d / width, alpha, column);
        write_quad(marks, &last, &next, style.color);
        marks.last[i] = Some(next);
    }
    for c in marks.chunks.iter_mut().filter(|c| c.dirty) {
        c.dirty = false;
        if let Some(mut m) = meshes.get_mut(&c.mesh) {
            m.insert_attribute(Mesh::ATTRIBUTE_POSITION, c.positions.clone());
            m.insert_attribute(Mesh::ATTRIBUTE_UV_0, c.uvs.clone());
            m.insert_attribute(Mesh::ATTRIBUTE_COLOR, c.colors.clone());
        }
    }
}

fn edge(centre: Vec3, dir: Vec3, normal: Vec3, width: f32, v: f32, alpha: f32, column: u32) -> Edge {
    let along = dir.reject_from(normal).normalize_or(Vec3::NEG_Z);
    let right = along.cross(normal).normalize_or(Vec3::X) * (0.5 * width.max(0.1));
    Edge { left: centre - right, right: centre + right, centre, v, alpha, style_column: column }
}

fn write_quad(marks: &mut SkidMarks, a: &Edge, b: &Edge, color: [f32; 3]) {
    let q = marks.cursor;
    marks.cursor = (marks.cursor + 1) % (CHUNKS * SEGS_PER_CHUNK);
    let chunk = &mut marks.chunks[q / SEGS_PER_CHUNK];
    let base = (q % SEGS_PER_CHUNK) * 4;
    // Half a texel in from each column edge so the clamp doesn't bleed the neighbour.
    let col = b.style_column as f32;
    let (u0, u1) = ((col * 128.0 + 0.5) / 512.0, ((col + 1.0) * 128.0 - 0.5) / 512.0);
    let rgba = |alpha: f32| [color[0], color[1], color[2], alpha];
    chunk.positions[base..base + 4].copy_from_slice(&[a.left.to_array(), a.right.to_array(), b.left.to_array(), b.right.to_array()]);
    chunk.uvs[base..base + 4].copy_from_slice(&[[u0, a.v], [u1, a.v], [u0, b.v], [u1, b.v]]);
    chunk.colors[base..base + 4].copy_from_slice(&[rgba(a.alpha), rgba(a.alpha), rgba(b.alpha), rgba(b.alpha)]);
    chunk.dirty = true;
}

/// Miles driven above the dirt accumulator's minimum speed, per car (reset on a car change).
#[derive(Resource, Default)]
pub struct CarDirt {
    pub miles: f32,
    car: String,
    sent: f32,
}

impl CarDirt {
    pub fn level(&self) -> f32 {
        (self.miles / DIRT_FULL_MILES).min(DIRT_MAX)
    }
}

fn accumulate_dirt(time: Res<Time>, mut dirt: ResMut<CarDirt>, cars: Query<&Car>, globals: Option<ResMut<fh1_render::FxCarGlobals>>) {
    let Ok(car) = cars.single() else { return };
    let v = &car.0;
    if dirt.car != v.data.media_name {
        *dirt = CarDirt { car: v.data.media_name.clone(), sent: -1.0, ..default() };
    }
    let speed = v.speed();
    if speed >= DIRT_MIN_SPEED {
        dirt.miles += speed * time.delta_secs() / 1609.344;
    }
    let level = dirt.level();
    // The bank re-uploads on every set: only send visible changes.
    if (level - dirt.sent).abs() < 0.002 {
        return;
    }
    let Some(mut g) = globals else { return };
    let mut a = g.get("alphaDirtAmount").unwrap_or(Vec4::new(1.0, 0.0, 0.0, 0.0));
    a.y = level;
    if g.set_vec("alphaDirtAmount", a) {
        dirt.sent = level;
    }
}
