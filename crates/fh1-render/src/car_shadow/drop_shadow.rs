//! The game's car contact shadow, `CDropShader` (car+0x7E0; docs/SHADERS.md, docs/SHADOWS.md "Car", RE scratch
//! re/out/dropshadow/NOTES.md). Drawn for every car whatever the CSM does. `FH1_DROPSHADOW=0` = off.
//!
//! The game renders the car top-down (ortho, `dropShadow*` techniques) into a 64² "DropShadow" target, blurs it,
//! and lays it on the ground as a 5-vertex fan fitted to the four wheel ground points, limited to the ground by a
//! stencil box. Here:
//! - The silhouette target is built on the CPU: the body's top-down coverage is rasterised once per car from the
//!   game-shaded parts' own triangles, and each frame (when its inputs change) the 64² image gets the car PS math
//!   (#451/453/455: falloff by |ndc| and opacity from the height h) plus the four tyre footprints (WheelScale,
//!   WheelOffset, WheelOpacity, steer). Then the blur.
//! - The ground fan is a depth-tested mesh lifted a little off the fitted plane: Bevy's main depth target has no
//!   stencil, so the stencil box is replaced by the depth test (on bumpy ground the fan can sink into the surface
//!   or float above dips, where the game's box would follow the surface).
//! - The PS (`drop_shadow.wgsl`) is DropShadow.fxobj's, translated by hand.
//!
//! Engine hook: put [`FxDropShadow`] on the car entity whose `Transform` is the car pose, and refresh it every
//! frame from the vehicle (ground points, wheels, height).

use std::path::Path;

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::primitives::Aabb;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::light::NotShadowCaster;
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::mesh::VertexAttributeValues;
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, Extent3d, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
    TextureDimension, TextureFormat,
};
use bevy::shader::ShaderRef;

use crate::car::FxCarBody;
use crate::car_material::FxCarMaterial;

/// Silhouette target size: 64² in game (256² when renderer+0x22F8 == 1, table 0x822DB218, meaning untraced).
const RT: usize = 64;
/// Top-down body coverage grid (per car, built once).
const MASK: usize = 256;
/// Lift of the ground fan along its normal (m) so the depth test keeps it above the road (not in the game,
/// which draws it with Z off inside its stencil box).
const LIFT: f32 = 0.02;

pub struct DropShadowPlugin;

impl Plugin for DropShadowPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FH1_DROPSHADOW").is_ok_and(|v| v == "0") {
            return;
        }
        embedded_asset!(app, "drop_shadow.wgsl");
        app.add_plugins(MaterialPlugin::<DropShadowMaterial>::default()).add_systems(
            PostUpdate,
            (collect_silhouettes, update_drop_shadows).chain().after(bevy::transform::TransformSystems::Propagate),
        );
    }
}

// ---------------------------------------------------------------- engine hook

/// Drop shadow inputs, on the car entity whose `Transform` is the (render) pose. All positions are in that
/// entity's local space (+Y up, front −Z). Wheels: LF, RF, LR, RR.
#[derive(Component, Clone, Copy, Default, Debug)]
pub struct FxDropShadow {
    pub wheels: [FxDropShadowWheel; 4],
    /// Body height above the ground (m), CDropShader+0x190 (set through car vfunc 0x82D78D78; how the game measures
    /// it is untraced: the engine passes the mean tyre-to-ground gap, 0 on the ground, INFERRED).
    pub height: f32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct FxDropShadowWheel {
    /// Wheel centre after the suspension.
    pub hub: Vec3,
    pub radius: f32,
    /// Tyre width (m).
    pub width: f32,
    /// Steer angle (rad about +Y).
    pub steer: f32,
    /// Ground point below the wheel.
    pub ground: Vec3,
    /// Corner alpha target: 1 on the ground, fading out with the gap (`calcDropShadowDisplacement` plane mode:
    /// 1 − 4·gap, INFERRED for the wheel points), 0 without ground.
    pub alpha: f32,
}

// ---------------------------------------------------------------- settings

/// GlobalCarAttribs `DropShadowSettings` (parser 0x82D91088) + TrackSettings `DropShadowColor` /
/// `DropShadowOpacityAdjustment`. Defaults = the retail disc's XML values (VERIFIED).
#[derive(Clone, Copy, Debug)]
pub struct DropShadowSettings {
    pub max_height: f32,
    pub falloff_start: f32,
    pub falloff_stop: f32,
    pub falloff_stop_scale: f32,
    pub width_max: f32,
    pub width_min: f32,
    pub length_max: f32,
    pub length_min: f32,
    pub opacity_max: f32,
    pub opacity_min: f32,
    pub wheel_scale: f32,
    pub wheel_offset: f32,
    pub wheel_opacity: f32,
    pub colour: Vec3,
    pub opacity_adjust: f32,
}

impl Default for DropShadowSettings {
    fn default() -> Self {
        Self {
            max_height: 0.3,
            falloff_start: 0.85,
            falloff_stop: 0.6,
            falloff_stop_scale: 0.15,
            width_max: 1.09,
            width_min: 1.06,
            length_max: 1.10,
            length_min: 1.05,
            opacity_max: 0.6,
            opacity_min: 0.8,
            wheel_scale: 0.55,
            wheel_offset: -0.12,
            wheel_opacity: 0.8,
            colour: Vec3::ZERO,
            opacity_adjust: 0.0,
        }
    }
}

fn xml_attr(xml: &str, tag: &str, a: &str) -> Option<f32> {
    let s = xml.find(&format!("<{tag} "))?;
    let t = &xml[s..s + xml[s..].find('>')?];
    let pat = format!(" {a}=\"");
    let i = t.find(&pat)? + pat.len();
    t[i..i + t[i..].find('"')?].trim().parse().ok()
}

impl DropShadowSettings {
    /// From `cars/shared/GlobalCarAttribs.xml` and `tracks/<track>/TrackSettings.xml` (missing values keep the defaults).
    pub fn load(assets: &Path, track: &str) -> Self {
        let mut s = Self::default();
        if let Ok(x) = std::fs::read_to_string(assets.join("cars/shared/GlobalCarAttribs.xml")) {
            let f = |a: &str, d: f32| xml_attr(&x, "DropShadowSettings", a).unwrap_or(d);
            s.max_height = f("MaxHeightAboveGround", s.max_height);
            s.falloff_start = f("FallOffCurveStartPos", s.falloff_start);
            s.falloff_stop = f("FallOffCurveStopPos", s.falloff_stop);
            s.falloff_stop_scale = f("FallOffCurveStopScaleValue", s.falloff_stop_scale);
            s.width_max = f("BodyWidthScaleForMaxHeightAboveGround", s.width_max);
            s.width_min = f("BodyWidthScaleForMinHeightAboveGround", s.width_min);
            s.length_max = f("BodyLengthScaleForMaxHeightAboveGround", s.length_max);
            s.length_min = f("BodyLengthScaleForMinHeightAboveGround", s.length_min);
            s.opacity_max = f("OpacityForMaxHeightAboveGround", s.opacity_max);
            s.opacity_min = f("OpacityForMinHeightAboveGround", s.opacity_min);
            s.wheel_scale = f("WheelScale", s.wheel_scale);
            s.wheel_offset = f("WheelOffset", s.wheel_offset);
            s.wheel_opacity = f("WheelOpacity", s.wheel_opacity);
        }
        if let Ok(x) = std::fs::read_to_string(assets.join("tracks").join(track).join("TrackSettings.xml")) {
            if let (Some(r), Some(g), Some(b)) = (xml_attr(&x, "DropShadowColor", "r"), xml_attr(&x, "DropShadowColor", "g"), xml_attr(&x, "DropShadowColor", "b")) {
                s.colour = Vec3::new(r, g, b);
            }
            s.opacity_adjust = xml_attr(&x, "DropShadowOpacityAdjustment", "value").unwrap_or(0.0);
        }
        s
    }

    /// t = sat(h / MaxHeightAboveGround) (getters 0x8247A4B8 etc., VERIFIED).
    fn t(&self, h: f32) -> f32 {
        (h / self.max_height).clamp(0.0, 1.0)
    }

    /// (WidthScale 0x8247A4B8, LengthScale 0x823E1CE8, FalloffScale 0x8247A500, Opacity 0x8247A540, WheelOpacity 0x823E1F70).
    fn at(&self, h: f32) -> (f32, f32, f32, f32, f32) {
        let t = self.t(h);
        let adjust = |o: f32| {
            let a = self.opacity_adjust;
            (if a >= 0.0 { o + a * (1.0 - o) } else { o * (1.0 + a) }).clamp(0.0, 1.0)
        };
        (
            self.width_min + (self.width_max - self.width_min) * t,
            self.length_min + (self.length_max - self.length_min) * t,
            1.0 + (self.falloff_stop_scale - 1.0) * t,
            adjust(self.opacity_min + (self.opacity_max - self.opacity_min) * t),
            adjust(self.wheel_opacity),
        )
    }
}

// ---------------------------------------------------------------- material

/// Change guards on the per-frame mesh / material writes (FH1_DROPSHADOW_GUARD=0 = write every frame, as before).
fn guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_DROPSHADOW_GUARD").is_ok_and(|v| v == "0"))
}

/// `drop_shadow.wgsl` `DropShadowParams`.
#[derive(Clone, Copy, ShaderType, Debug, Default)]
pub struct DropShadowParams {
    /// rgb = DropShadowColor (PS c0), w = 1 to write raw (FH1 post chain on).
    pub colour: Vec4,
    /// xy = fadeDistance (PS c1) = (0.05, 0.05·halfX/halfZ), z = psSceneShadowFadeParams.z.
    pub fade: Vec4,
    /// psFogColor (PS c2).
    pub fog_colour: Vec4,
    /// vsFogParameters (VS c14): x = start, w = density.
    pub fog: Vec4,
    /// vsSceneShadowFadeParams (VS c16) xy.
    pub scene_fade: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct DropShadowMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    #[uniform(2)]
    pub params: DropShadowParams,
}

impl Material for DropShadowMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_render/car_shadow/drop_shadow.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_render/car_shadow/drop_shadow.wgsl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, _: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        d.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(2),
        ])?];
        // Fan: CULL = 6 (CCW) in game; drawn two-sided here (the fan's winding follows the fitted corners).
        d.primitive.cull_mode = None;
        // SRCALPHA / INVSRCALPHA, Z write off (VERIFIED, 0x82443600); SEPARATEALPHABLEND SRCBLENDALPHA 6 / DESTBLENDALPHA 1.
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::OneMinusSrcAlpha, operation: BlendOperation::Add },
                    alpha: BlendComponent { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                });
            }
        }
        if let Some(ds) = d.depth_stencil.as_mut() {
            ds.depth_write_enabled = Some(false).into();
            // Stand-in for the stencil box: depth-tested, pulled toward the camera (reverse Z).
            ds.bias.constant = 4;
            ds.bias.slope_scale = 1.0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- per car state

/// Top-down body silhouette and the drawing state of one car.
#[derive(Component)]
struct DropShadowState {
    /// Body triangles projected on the car's XZ plane (car-local).
    tris: Vec<[Vec2; 3]>,
    min: Vec3,
    max: Vec3,
    /// MASK² coverage over min.xz..max.xz (built when `tris` changed).
    mask: Vec<u8>,
    dirty: bool,
    image: Handle<Image>,
    mesh: Handle<Mesh>,
    material: Handle<DropShadowMaterial>,
    /// Smoothed corner alphas (FR, FL, RL, RR order of the fan's v1..v4).
    alpha: [f32; 4],
    /// Inputs of the last silhouette image (redrawn when they change).
    key: Option<[i32; 9]>,
    settings: DropShadowSettings,
    /// The ground fan entity, and whether the distance cull hides it.
    fan: Entity,
    far: bool,
}

/// Drop shadows of cars farther than this from the main camera (m) are hidden and not updated (P8-B: traffic / AI cars
/// rewrote their fan mesh, and their silhouette when the suspension moved, every frame at any distance; a 64² contact
/// shadow is a few pixels there). `FH1_DROPSHADOW_DIST=m` (80), 0 = old (always). 10 m hysteresis.
fn cull_dist() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_DROPSHADOW_DIST").ok().and_then(|v| v.parse().ok()).unwrap_or(80.0))
}
const CULL_HYSTERESIS: f32 = 10.0;

/// Find the entity with [`FxDropShadow`] above `e`.
fn car_of(mut e: Entity, parents: &Query<&ChildOf>, cars: &Query<(), With<FxDropShadow>>) -> Option<Entity> {
    for _ in 0..16 {
        if cars.contains(e) {
            return Some(e);
        }
        e = parents.get(e).ok()?.parent();
    }
    None
}

/// New game-shaded body parts: add their top-down triangles to their car's silhouette. Runs the frame the parts
/// spawn (their meshes are RENDER_WORLD only, so they must be read before the extract), after transform propagation.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn collect_silhouettes(
    mut commands: Commands,
    new: Query<(Entity, &Mesh3d, &Aabb, &GlobalTransform), (Added<MeshMaterial3d<FxCarMaterial>>, Without<super::FxWheelPart>)>,
    parents: Query<&ChildOf>,
    car_marks: Query<(), With<FxDropShadow>>,
    mut cars: Query<(&GlobalTransform, Option<&mut DropShadowState>), With<FxDropShadow>>,
    bodies: Query<&FxCarBody>,
    children: Query<&Children>,
    mut images: ResMut<Assets<Image>>,
    mut mesh_assets: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<DropShadowMaterial>>,
    mut pending: Local<Vec<(Entity, Vec<[Vec2; 3]>, Vec3, Vec3)>>,
) {
    pending.clear();
    if !new.is_empty() && std::env::var_os("FH1_DROPSHADOW_LOG").is_some() {
        let n = new.iter().filter(|(e, ..)| car_of(*e, &parents, &car_marks).is_some()).count();
        let m = new.iter().filter(|(_, mesh, ..)| mesh_assets.get(&mesh.0).is_some()).count();
        info!("drop shadow: {} new car parts, {n} under an FxDropShadow car, {m} with mesh data", new.iter().count());
    }
    for (e, mesh, aabb, gt) in &new {
        let Some(car) = car_of(e, &parents, &car_marks) else { continue };
        let Ok((car_gt, _)) = cars.get(car) else { continue };
        let Some(pos) = mesh_assets.get(&mesh.0).and_then(|m| crate::car_shadow::unpack_positions(m, aabb).zip(m.indices().map(|i| i.iter().collect::<Vec<_>>()))) else { continue };
        let to_car = car_gt.affine().inverse() * gt.affine();
        let local: Vec<Vec3> = pos.0.iter().map(|&p| to_car.transform_point3(p)).collect();
        let (mut lo, mut hi) = (Vec3::MAX, Vec3::MIN);
        for p in &local {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        let tris: Vec<[Vec2; 3]> = pos.1.chunks_exact(3).map(|t| [local[t[0]].xz(), local[t[1]].xz(), local[t[2]].xz()]).collect();
        // One entry per car: the state insert below is deferred, so parts arriving together must be merged first.
        match pending.iter_mut().find(|p| p.0 == car) {
            Some(p) => {
                p.1.extend(tris);
                p.2 = p.2.min(lo);
                p.3 = p.3.max(hi);
            }
            None => pending.push((car, tris, lo, hi)),
        }
    }
    for (car, tris, lo, hi) in pending.drain(..) {
        let Ok((_, state)) = cars.get_mut(car) else { continue };
        if let Some(mut s) = state {
            s.tris.extend(tris);
            s.min = s.min.min(lo);
            s.max = s.max.max(hi);
            s.dirty = true;
            continue;
        }
        // First part of this car: the silhouette image, the fan mesh and its material.
        let settings = children
            .iter_descendants(car)
            .find_map(|d| bodies.get(d).ok())
            .map(|b| DropShadowSettings::load(&b.assets, &b.track))
            .unwrap_or_default();
        let mut img = Image::new(
            Extent3d { width: RT as u32, height: RT as u32, depth_or_array_layers: 1 },
            TextureDimension::D2,
            vec![0; RT * RT],
            TextureFormat::R8Unorm,
            RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
        );
        // ADDRESSU/V = clamp, MAG/MIN linear, no mips (VERIFIED, sampler states in 0x82443600).
        img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::ClampToEdge,
            address_mode_v: ImageAddressMode::ClampToEdge,
            mag_filter: ImageFilterMode::Linear,
            min_filter: ImageFilterMode::Linear,
            ..default()
        });
        let image = images.add(img);
        let mesh = mesh_assets.add(fan_mesh());
        let material = materials.add(DropShadowMaterial { texture: image.clone(), params: DropShadowParams::default() });
        let fan = commands
            .spawn((
                Mesh3d(mesh.clone()),
                MeshMaterial3d(material.clone()),
                Transform::IDENTITY,
                Visibility::Inherited,
                NoFrustumCulling,
                NotShadowCaster,
                ChildOf(car),
            ))
            .id();
        commands.entity(car).insert(DropShadowState {
            tris,
            min: lo,
            max: hi,
            mask: Vec::new(),
            dirty: true,
            image,
            mesh,
            material,
            alpha: [0.0; 4],
            key: None,
            settings,
            fan,
            far: false,
        });
    }
}

/// Fan of 4 triangles around the centre (the game's IB 1,2,0 2,3,0 3,4,0 4,1,0; UVs v0 (.5,.5) v1 (1,0) v2 (0,0)
/// v3 (0,1) v4 (1,1), VERIFIED ctor 0x83148E40). Positions are written per frame in world space.
fn fan_mesh() -> Mesh {
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD);
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; 5]);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.5f32, 0.5], [1.0, 0.0], [0.0, 0.0], [0.0, 1.0], [1.0, 1.0]]);
    m.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![[1.0f32; 4]; 5]);
    m.insert_indices(Indices::U16(vec![1, 2, 0, 2, 3, 0, 3, 4, 0, 4, 1, 0]));
    m
}

/// Rasterise the projected triangles into a MASK² coverage grid over min.xz..max.xz (cell centres).
fn build_mask(tris: &[[Vec2; 3]], min: Vec2, max: Vec2) -> Vec<u8> {
    let mut mask = vec![0u8; MASK * MASK];
    let size = (max - min).max(Vec2::splat(1e-3));
    let to_grid = |p: Vec2| (p - min) / size * MASK as f32;
    for t in tris {
        let [a, b, c] = t.map(to_grid);
        let area = (b - a).perp_dot(c - a);
        if area.abs() < 1e-6 {
            continue;
        }
        let lo = a.min(b).min(c).floor().max(Vec2::ZERO);
        let hi = a.max(b).max(c).ceil().min(Vec2::splat(MASK as f32));
        for y in lo.y as usize..hi.y as usize {
            for x in lo.x as usize..hi.x as usize {
                let p = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                let (w0, w1, w2) = ((c - b).perp_dot(p - b), (a - c).perp_dot(p - c), (b - a).perp_dot(p - a));
                if (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0) {
                    mask[y * MASK + x] = 255;
                }
            }
        }
    }
    mask
}

fn smoothstep01(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Blur of the silhouette target, 0x8246D400(σ 3.0, x scale 1.0, y scale halfX/halfZ, 1 iteration) on the 64²
/// target (0x82DC2AC8, 17 taps σ 5, on the 256² one). Kernel table 0x8246D650 modes 0x11/0x12: 9 taps at 0, ±1..±4
/// texels, weight exp(−i²/2σ²)/√(2πσ²) (0x82E2D148), normalised to 1 (0x823E17F0); horizontal pass, then the
/// offsets move to v and are scaled by halfX/halfZ, so the vertical step is one texel's width in metres (VERIFIED;
/// which call argument is σ vs the scales is INFERRED from the register order).
fn blur(img: &mut [f32], v_scale: f32) {
    const SIGMA: f32 = 3.0;
    let w: [f32; 9] = std::array::from_fn(|k| {
        let i = k as f32 - 4.0;
        (-(i * i) / (2.0 * SIGMA * SIGMA)).exp()
    });
    let total: f32 = w.iter().sum();
    let at = |src: &[f32], x: i32, y: i32| src[y.clamp(0, RT as i32 - 1) as usize * RT + x.clamp(0, RT as i32 - 1) as usize];
    let mut tmp = vec![0.0f32; RT * RT];
    for y in 0..RT as i32 {
        for x in 0..RT as i32 {
            tmp[y as usize * RT + x as usize] = (0..9).map(|k| w[k] * at(img, x + k as i32 - 4, y)).sum::<f32>() / total;
        }
    }
    for y in 0..RT as i32 {
        for x in 0..RT as i32 {
            let mut s = 0.0;
            for (k, wk) in w.iter().enumerate() {
                // Fractional offsets: bilinear (clamp addressing).
                let o = (k as f32 - 4.0) * v_scale + y as f32;
                let (y0, f) = (o.floor() as i32, o - o.floor());
                s += wk * (at(&tmp, x, y0) * (1.0 - f) + at(&tmp, x, y0 + 1) * f);
            }
            img[y as usize * RT + x as usize] = s / total;
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_drop_shadows(
    time: Res<Time>,
    mut cars: Query<(&FxDropShadow, &GlobalTransform, &mut DropShadowState)>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<DropShadowMaterial>>,
    lib: Option<Res<crate::FxLibrary>>,
    globals: Option<Res<crate::FxGlobals>>,
    car_globals: Option<Res<crate::FxCarGlobals>>,
    cams: Query<&GlobalTransform, With<crate::post::FxPostCamera>>,
    mut vis: Query<&mut Visibility>,
) {
    let dt = time.delta_secs().min(0.1);
    let raw = lib.is_some_and(|l| l.raw_output);
    let dist = cull_dist();
    let eye = cams.iter().next().map(|t| t.translation());
    for (input, gt, mut s) in &mut cars {
        let s = &mut *s;
        if let (true, Some(eye)) = (dist > 0.0, eye) {
            let d = eye.distance(gt.translation());
            let far = if s.far { d > dist - CULL_HYSTERESIS } else { d > dist };
            if far != s.far {
                s.far = far;
                if let Ok(mut v) = vis.get_mut(s.fan) {
                    *v = if far { Visibility::Hidden } else { Visibility::Inherited };
                }
            }
            if far {
                // Redrawn from the current inputs when it comes back.
                s.key = None;
                continue;
            }
        }
        if s.dirty {
            s.mask = build_mask(&s.tris, s.min.xz(), s.max.xz());
            s.dirty = false;
            s.key = None;
        }
        let set = s.settings;
        // Body box (car+0x940/0x944 half extents, centre car+0x950; ctor 0x83148E40 takes them from the bbox car+0x10A0).
        let centre = (s.min + s.max) * 0.5;
        let half = ((s.max - s.min) * 0.5).max(Vec3::splat(0.05));
        let h = input.height.max(0.0);
        let (ws, ls, falloff, opacity, wheel_opacity) = set.at(h);

        // ---- silhouette target (0x823F5A20) ----
        let q = |v: f32, k: f32| (v * k).round() as i32;
        let key = [
            q(h, 200.0),
            q(input.wheels[0].steer, 200.0),
            q(input.wheels[1].steer, 200.0),
            q(input.wheels[0].hub.y, 100.0),
            q(input.wheels[1].hub.y, 100.0),
            q(input.wheels[2].hub.y, 100.0),
            q(input.wheels[3].hub.y, 100.0),
            q(input.wheels[0].radius, 1000.0),
            q(input.wheels[0].width, 1000.0),
        ];
        if s.key != Some(key) {
            s.key = Some(key);
            let (w, l) = (half.x * ws, half.z * ls);
            // 64² target: WheelOffset + 0.08 while drawing (VERIFIED, restored after).
            let wheel_offset = set.wheel_offset + 0.08;
            // Tyre footprints seen from above: the wheel matrix scale is (tyreWidth·1.05, WheelScale·r, WheelScale·r)
            // and the wheel moves by −WheelOffset along its axle (VERIFIED, 0x82419A30); the direction along the
            // axle (outward) is INFERRED.
            let wheels: Vec<(Vec2, Vec2, Vec2)> = input
                .wheels
                .iter()
                .enumerate()
                .map(|(i, wh)| {
                    let side = if i % 2 == 0 { -1.0 } else { 1.0 };
                    let (sin, cos) = wh.steer.sin_cos();
                    // Rotation about +Y: local X axis -> (cos, -sin) on (x, z).
                    let ax = Vec2::new(cos, -sin);
                    let c = wh.hub.xz() + ax * (-wheel_offset) * side;
                    (c, ax, Vec2::new(wh.width * 1.05 * 0.5, set.wheel_scale * wh.radius))
                })
                .collect();
            let msize = (s.max.xz() - s.min.xz()).max(Vec2::splat(1e-3));
            let mut img = vec![0.0f32; RT * RT];
            const SUB: [f32; 2] = [0.25, 0.75];
            for y in 0..RT {
                for x in 0..RT {
                    let (mut body, mut wheel) = (0.0f32, 0.0f32);
                    for sy in SUB {
                        for sx in SUB {
                            let ndc = Vec2::new((x as f32 + sx) / RT as f32 * 2.0 - 1.0, (y as f32 + sy) / RT as f32 * 2.0 - 1.0);
                            let p = Vec2::new(centre.x + ndc.x * w, centre.z + ndc.y * l);
                            let g = (p - s.min.xz()) / msize * MASK as f32;
                            if g.x >= 0.0 && g.y >= 0.0 && (g.x as usize) < MASK && (g.y as usize) < MASK && s.mask[g.y as usize * MASK + g.x as usize] != 0 {
                                body += 0.25;
                            }
                            for &(c, ax, hs) in &wheels {
                                let d = p - c;
                                let az = ax.perp();
                                if d.dot(ax).abs() <= hs.x && d.dot(az).abs() <= hs.y {
                                    wheel += 0.25;
                                    break;
                                }
                            }
                        }
                    }
                    // Car PS #451: per-axis falloff toward FalloffScale past FallOffCurveStopPos (|ndc.y| × 0.8).
                    let ndc = Vec2::new((x as f32 + 0.5) / RT as f32 * 2.0 - 1.0, (y as f32 + 0.5) / RT as f32 * 2.0 - 1.0);
                    let span = set.falloff_start - set.falloff_stop;
                    let axis = |v: f32| {
                        let t = if span.abs() < 1e-6 { 0.0 } else { ((v - set.falloff_stop) / span).clamp(0.0, 1.0) };
                        1.0 + (falloff - 1.0) * smoothstep01(t)
                    };
                    let v = axis(ndc.x.abs()).min(axis(0.8 * ndc.y.abs())) * opacity;
                    // The body is drawn over the wheels (top-down, depth-tested; INFERRED).
                    img[y * RT + x] = body * v + (1.0 - body) * wheel * wheel_opacity;
                }
            }
            blur(&mut img, half.x / half.z);
            if std::env::var_os("FH1_DROPSHADOW_LOG").is_some() {
                let peak = img.iter().cloned().fold(0.0f32, f32::max);
                info!("drop shadow: {} tris, box {:?}..{:?}, h {h:.3}, peak {peak:.2}, ground {:?}, alpha {:?}", s.tris.len(), s.min, s.max, input.wheels.map(|w| w.ground), input.wheels.map(|w| w.alpha));
            }
            if let Some(mut i) = images.get_mut(&s.image) {
                i.data = Some(img.iter().map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect());
            }
        }

        // ---- ground fan (CalcDropShadowDisplacement 0x831486B8) ----
        let g = input.wheels.map(|w| w.ground);
        let front = (g[0] + g[1]) * 0.5;
        let rear = (g[2] + g[3]) * 0.5;
        let along = (front - rear).normalize_or(Vec3::NEG_Z);
        let lat = ((g[1] - g[0]) + (g[3] - g[2])).reject_from(along).normalize_or(Vec3::X);
        // The quad is centred on the body box: wheel midpoint moved by the box centre's offset from it (INFERRED
        // from the +0x194 + +0x174 length offset).
        let mid = (front + rear) * 0.5;
        let hub_mid = input.wheels.iter().map(|w| w.hub).sum::<Vec3>() * 0.25;
        let off = centre - hub_mid;
        let c = mid + lat * off.x + along * -off.z;
        let (hx, hz) = (half.x * 1.05, half.z);
        let n = lat.cross(along).normalize_or(Vec3::Y);
        let n = if n.y < 0.0 { -n } else { n };
        let lift = n * LIFT;
        // v1 FR (u 1, v 0 = front), v2 FL, v3 RL, v4 RR; the silhouette's v runs front (0) to back (1).
        let corners = [c + along * hz + lat * hx, c + along * hz - lat * hx, c - along * hz - lat * hx, c - along * hz + lat * hx];
        // Corner alphas move toward their wheel's target at 8/s (timer +0x198, VERIFIED rate; per-wheel target INFERRED).
        let target = [input.wheels[1].alpha, input.wheels[0].alpha, input.wheels[2].alpha, input.wheels[3].alpha];
        for k in 0..4 {
            let d = target[k] - s.alpha[k];
            s.alpha[k] += d.clamp(-8.0 * dt, 8.0 * dt);
        }
        let to_world = gt.affine();
        let mut pos = vec![to_world.transform_point3(c + lift).to_array()];
        pos.extend(corners.iter().map(|p| to_world.transform_point3(*p + lift).to_array()));
        let mean = s.alpha.iter().sum::<f32>() * 0.25;
        let mut cols = vec![[1.0, 1.0, 1.0, mean]];
        cols.extend(s.alpha.iter().map(|a| [1.0, 1.0, 1.0, *a]));
        // Written only when they change (2026-10-08 perf: a parked car rewrote mesh + material every frame, and every
        // Modified asset is re-extracted and re-allocated by the render world). FH1_DROPSHADOW_GUARD=0 = always write.
        let same_mesh = guard()
            && meshes.get(&s.mesh).is_some_and(|m| {
                let eq = |id: bevy::mesh::MeshVertexAttribute, v: &[[f32; 3]]| matches!(m.try_attribute(id), Ok(VertexAttributeValues::Float32x3(a)) if a.as_slice() == v);
                let eq4 = |id: bevy::mesh::MeshVertexAttribute, v: &[[f32; 4]]| matches!(m.try_attribute(id), Ok(VertexAttributeValues::Float32x4(a)) if a.as_slice() == v);
                eq(Mesh::ATTRIBUTE_POSITION, &pos) && eq4(Mesh::ATTRIBUTE_COLOR, &cols)
            });
        if !same_mesh {
            if let Some(mut m) = meshes.get_mut(&s.mesh) {
                m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
                m.insert_attribute(Mesh::ATTRIBUTE_COLOR, cols);
            }
        }

        // ---- PS / VS constants (0x82443600) ----
        let fog = globals.as_ref().and_then(|g| g.get("vsFogParameters")).unwrap_or(Vec4::ZERO);
        let fog_colour = car_globals.as_ref().and_then(|g| g.get("psFogColor")).unwrap_or(Vec4::ZERO);
        let params = DropShadowParams {
            colour: set.colour.extend(if std::env::var_os("FH1_DROPSHADOW_DEBUG").is_some() { 2.0 } else { raw as u32 as f32 }),
            fade: Vec4::new(0.05, 0.05 * hx / hz, 1.0, 0.0),
            fog_colour,
            fog,
            // vsSceneShadowFadeParams default (0, 1, 1, 0): the CSM-range fade (×1.5 beyond 0.9·D) only matters
            // far from the camera, where the car never is in free roam.
            scene_fade: Vec4::new(0.0, 1.0, 1.0, 0.0),
        };
        let same_params = guard()
            && materials.get(&s.material).is_some_and(|m| {
                let o = &m.params;
                o.colour == params.colour && o.fade == params.fade && o.fog_colour == params.fog_colour && o.fog == params.fog && o.scene_fade == params.scene_fade
            });
        if !same_params {
            if let Some(mut m) = materials.get_mut(&s.material) {
                m.params = params;
            }
        }
    }
}
