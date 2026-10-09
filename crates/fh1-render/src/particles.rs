//! FH1 particles (docs/EFFECTS.md): the effects.zip emitters (`<Effect>` XML, fh1setup `effects` group) simulated on
//! the CPU and drawn as camera-facing quads, one draw per effect, sorted back to front inside it.
//!
//! The game's particle shader (`media/shaders/v2/effects/particle_smoke.fxobj`, VERIFIED by fxdump) does no lighting:
//! the VS passes position / colour / texcoord through (the quads are expanded on the CPU) and the PS is
//! `oC0 = (tex × colour − FogColor) × fog + FogColor`, alpha = tex.a × colour.a. So the lighting is in the vertex colour,
//! from the XML's `light_influence` (CPU side, scale INFERRED, see [`light_scale`]), and fog uses the scene's fog model
//! (docs/SHADERS.md "Fog model") per vertex here.
//!
//! Public API (also used by fh1-engine effects_surface.rs): [`FxParticles::effect`] loads an effect by its XML stem,
//! [`FxParticles::spawn`] starts particles, [`FxParticles::count`] turns a rate into a count. Spawners run in Update
//! or in PostUpdate before [`FxParticlesSet`]. No per-particle entities; the pools, the sort scratch and the vertex
//! arrays are preallocated per effect (budget × [`EMITTERS_PER_EFFECT`]) and reused every frame.
//!
//! `FH1_PARTICLES=0` turns the whole system off.

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexAttributeValues};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError};
use bevy::shader::ShaderRef;

/// Pool size per effect = the XML `budget` × this: the game's budget is per emitter instance (one per wheel).
pub const EMITTERS_PER_EFFECT: usize = 4;
/// Hard cap on live particles over all effects.
pub const MAX_LIVE_TOTAL: usize = 16_384;
/// Hard cap per effect pool.
const MAX_PER_EFFECT: usize = 4_096;
/// effects.zip `Gravity strength` → m/s² (INFERRED by FX3: Dirt1 200 falls at ~1 g; Dust1 −3 rises slowly).
pub const GRAVITY_UNIT: f32 = 0.049;
/// effects.zip angles (OffAxis / OffPlane) are degrees: the XML loader (default.xex 82D082D8) multiplies them by
/// 0.0174533 (82000d44) on load and by 57.2958 (82005600) on save (VERIFIED, Ghidra 2026-10-06).
const DEG: f32 = std::f32::consts::PI / 180.0;

/// `FH1_PARTICLE_SPREAD_RAD=1`: the old reading of OffAxis as radians (Smoke's 1.7° jet became a 98° cone).
fn spread_in_radians() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_PARTICLE_SPREAD_RAD").is_ok_and(|v| v == "1"))
}

/// `FH1_PARTICLE_LAG_FIX=0`: the old draw that set the batch Transform after propagation (the batch drew around last
/// frame's centroid and jumped when an effect restarted elsewhere) and lost the gradient flag on uniform updates.
fn lag_fix() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_PARTICLE_LAG_FIX").map_or(true, |v| v != "0"))
}

/// `FH1_PARTICLE_MAX_SCREEN=0`: ignore the XML `max_screen_size` cap.
fn max_screen_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_PARTICLE_MAX_SCREEN").map_or(true, |v| v != "0"))
}

/// `FH1_PARTICLE_LIGHT=<x>`: multiplier on the particle lighting (default 1; a tuning knob).
fn light_gain() -> f32 {
    static F: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_PARTICLE_LIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0))
}

/// `FH1_FX_HALF_MIN_SIZE=<m>` (default 0.2): effects whose largest nominal quad (`max(Size valueX, maxValueX)`) is at
/// least this go through the half-res effects pass (fx_half_res.rs, `FH1_FX_HALF_RES`); smaller ones stay sharp on the
/// full-res Material path. Of the shipped effects this sends Smoke (1.0), SmokeRim0..3 (0.46), Backfire (0.42), Dust1
/// (4.7), AMB_SparkCannon_Flare (1.0), AMB_Confetti_Smoke (2.0) and AMB_StartSparkCannon_Smoke (3.25) to half res and
/// keeps Dirt1 (0.018), Grass1 (0.035), GravelBits (0.02), Leaf1..5 (0.06), Litter (0.16), AMB_SparkCannon_Sparks (0.16)
/// and AMB_Confetti_Canon_Burst (0.14) at full res (sizes from the installed effects.zip XMLs, 2026-10-08).
fn half_min_size() -> f32 {
    static F: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_FX_HALF_MIN_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.2))
}

/// The effect is drawn by the half-res effects pass (see [`half_min_size`]).
fn goes_half_res(d: &EffectDef) -> bool {
    crate::fx_half_res::on() && d.size.max(d.size_max) >= half_min_size()
}

/// Soft-particle fade height above [`Spawn::ground_y`] (m). The game's `_soft` shader variant reads scene depth; the
/// engine has no depth prepass on the main view, so particles fade near the ground they were spawned on instead.
const SOFT_HEIGHT: f32 = 0.25;

pub struct FxParticlesPlugin;

/// Spawn particles in Update, or in PostUpdate `.before(FxParticlesSet)`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct FxParticlesSet;

impl Plugin for FxParticlesPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "particles.wgsl");
        app.add_plugins(MaterialPlugin::<ParticleMaterial>::default())
            .insert_resource(FxParticles::new())
            .add_systems(PostUpdate, (find_root, simulate, draw, stats).chain().in_set(FxParticlesSet).after(bevy::transform::TransformSystems::Propagate));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectId(pub u16);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Blend {
    Alpha,
    Add,
}

/// One `<Effect>` XML (effects.zip). Field names follow the XML; units as documented (GUESSED where marked).
#[derive(Clone, Debug)]
pub struct EffectDef {
    pub name: String,
    /// `Quantity`: particles per second at full intensity (INFERRED unit).
    pub rate: f32,
    /// `budget`: live particles per emitter.
    pub budget: u32,
    /// `Velocity` (m/s) and its relative variation.
    pub speed: f32,
    pub speed_var: f32,
    /// `OffAxis variation` (rad; the XML holds degrees): ± tilt away from the emit axis.
    pub spread: f32,
    /// `OffAxis value` (rad): the mean tilt; `OffPlane value` / `variation` (rad): the tilt's direction around the axis
    /// (3ds Max SuperSpray). With no mean tilt or no plane variation the direction around the axis is random.
    pub off_axis: f32,
    pub off_plane: f32,
    pub off_plane_var: f32,
    /// `ParticleType max_screen_size`: the largest quad as a fraction of the viewport height (0 = no cap; INFERRED
    /// reading, the game's use is untraced).
    pub max_screen: f32,
    /// `Life` (s) and its relative variation.
    pub life: f32,
    pub life_var: f32,
    /// `Size valueX` (m, full quad width; GUESSED), its relative variation, `maxValueX`, `growforX` (life fraction to grow
    /// from size to size_max; GUESSED), `endValueX` / `shrinkforX` (shrink toward end over the last life fraction).
    pub size: f32,
    pub size_var: f32,
    pub size_max: f32,
    pub grow_for: f32,
    pub size_end: f32,
    pub shrink_for: f32,
    /// `SpinTime` (s per turn; 0 = none) and variation, `SpinPhase` variation (rad).
    pub spin_time: f32,
    pub spin_time_var: f32,
    pub spin_phase_var: f32,
    /// `MotionInheritance` influence% × multiplier: the fraction of the emitter's velocity a particle keeps.
    pub inherit: f32,
    /// `Gravity strength` × [`GRAVITY_UNIT`] (m/s², + = falls).
    pub gravity: f32,
    /// `Friction strength`: velocity decay rate (1/s; GUESSED).
    pub friction: f32,
    /// `RGBStart` / `RGBMid` / `RGBEnd`.
    pub rgba: [Vec4; 3],
    /// `FadeInTo` / `FadeOutFrom` (life fractions): start→mid until fade_in, mid until fade_out, then →end.
    pub fade_in: f32,
    pub fade_out: f32,
    /// `light_influence` (x, y, z, w) and `dynamic_lighting`.
    pub light_influence: Vec4,
    pub dynamic_lighting: bool,
    pub blend: Blend,
    /// `Texture` / `Texture2` (effects.zip `.xds` names).
    pub texture: String,
    pub gradient: String,
    /// Atlas columns × rows from the texture name (`GR_Dirt_8X1_DIFF` = 8 × 1); 1 × 1 otherwise.
    pub frames: (u8, u8),
    /// `ParticleShaderType` (`_soft`, `_animated_rowonly`, `_animated_rowonly_gradient`, ...).
    pub shader: String,
    /// `Size valueY / valueX`: quad height over width (1 = square). Backfire = 0.4 / 0.2 (its atlas frames are 64 x 128).
    pub aspect: f32,
    /// `SpinAxis type="PerpToDirOfTravel"`: the quad's up (texture top) follows the particle's emission direction on
    /// screen and its base sits at the particle (flames leave the pipe instead of standing upright over it).
    pub align_travel: bool,
}

impl EffectDef {
    /// Parse an effects.zip `<Effect>` XML.
    pub fn parse(name: &str, xml: &str) -> Self {
        let a = |tag: &str, attr: &str| xml_attr(xml, tag, attr);
        let f = |tag: &str, attr: &str, d: f32| a(tag, attr).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        let rgba = |tag: &str| Vec4::new(f(tag, "r", 1.0), f(tag, "g", 1.0), f(tag, "b", 1.0), f(tag, "a", 1.0));
        let texture = a("Texture", "value").unwrap_or_default();
        // Frames from the texture name (`_8X1_`), else `<Animation numFramesX numFramesY>` (Backfire: a 4 x 2 sheet that
        // was drawn whole, all eight flames on one quad).
        let mut frames = atlas_frames(&texture);
        if frames == (1, 1) {
            let (x, y) = (f("Animation", "numFramesX", 1.0) as u8, f("Animation", "numFramesY", 1.0) as u8);
            frames = (x.max(1), y.max(1));
        }
        let (size_x, size_y) = (f("Size", "valueX", 0.1), f("Size", "valueY", 0.0));
        Self {
            name: name.to_string(),
            rate: f("Quantity", "value", 0.0),
            budget: f("Effect", "budget", 100.0) as u32,
            speed: f("Velocity", "value", 0.0),
            speed_var: f("Velocity", "variation", 0.0),
            spread: (f("OffAxis", "variation", 0.0) * if spread_in_radians() { 1.0 } else { DEG }).clamp(0.0, std::f32::consts::PI),
            off_axis: f("OffAxis", "value", 0.0) * DEG,
            off_plane: f("OffPlane", "value", 0.0) * DEG,
            off_plane_var: f("OffPlane", "variation", 0.0) * DEG,
            max_screen: f("ParticleType", "max_screen_size", 0.0).max(0.0),
            life: f("Life", "value", 1.0).max(0.01),
            life_var: f("Life", "variation", 0.0),
            size: f("Size", "valueX", 0.1),
            size_var: f("Size", "variationX", 0.0),
            size_max: f("Size", "maxValueX", 0.0),
            grow_for: f("Size", "growforX", 0.0),
            size_end: f("Size", "endValueX", 0.0),
            shrink_for: f("Size", "shrinkforX", 0.0),
            spin_time: f("SpinTime", "value", 0.0),
            spin_time_var: f("SpinTime", "variation", 0.0),
            spin_phase_var: f("SpinPhase", "variation", 0.0),
            inherit: f("MotionInheritance", "influence", 0.0) * 0.01 * f("MotionInheritance", "multiplier", 0.0),
            gravity: f("Gravity", "strength", 0.0) * GRAVITY_UNIT,
            friction: f("Friction", "strength", 0.0),
            rgba: [rgba("RGBStart"), rgba("RGBMid"), rgba("RGBEnd")],
            fade_in: f("FadeInTo", "fadeInTo", 0.0),
            fade_out: f("FadeOutFrom", "fadeOutFrom", 1.0),
            light_influence: Vec4::new(f("light_influence", "v.x", 0.0), f("light_influence", "v.y", 1.0), f("light_influence", "v.z", 1.0), f("light_influence", "v.w", 1.0)),
            dynamic_lighting: f("ParticleType", "dynamic_lighting", 0.0) != 0.0,
            blend: if a("BlendType", "value").is_some_and(|b| b.eq_ignore_ascii_case("add") || b.eq_ignore_ascii_case("additive")) { Blend::Add } else { Blend::Alpha },
            texture,
            gradient: a("Texture2", "value").unwrap_or_default(),
            frames,
            shader: a("ParticleShaderType", "value").unwrap_or_default(),
            aspect: if size_x > 1e-4 && size_y > 1e-4 && aspect_on() { (size_y / size_x).clamp(0.25, 4.0) } else { 1.0 },
            align_travel: aspect_on() && a("SpinAxis", "type").is_some_and(|t| t == "PerpToDirOfTravel"),
        }
    }

    /// Colour and alpha at life fraction `t`.
    pub fn colour_at(&self, t: f32) -> Vec4 {
        let [s, m, e] = self.rgba;
        if t < self.fade_in {
            s.lerp(m, t / self.fade_in.max(1e-4))
        } else if t <= self.fade_out {
            m
        } else {
            m.lerp(e, ((t - self.fade_out) / (1.0 - self.fade_out).max(1e-4)).min(1.0))
        }
    }

    /// Quad width (m) at life fraction `t` for a particle born with `size0` / `size_max`.
    fn size_at(&self, t: f32, size0: f32, size_max: f32) -> f32 {
        let mut s = if self.grow_for > 0.0 && size_max > 0.0 { size0 + (size_max - size0) * (t / self.grow_for).min(1.0) } else { size0 };
        if self.shrink_for > 0.0 && t > 1.0 - self.shrink_for {
            s += (self.size_end - s) * ((t - (1.0 - self.shrink_for)) / self.shrink_for);
        }
        s.max(0.0)
    }
}

/// `<Tag ... attr="v">`: the first such tag's attribute.
fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = xml[from..].find(&format!("<{tag}")) {
        let at = from + i;
        let rest = &xml[at + tag.len() + 1..];
        from = at + 1;
        // `<Size` must not match `<SizeX`.
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let rest = &rest[..rest.find('>')?];
        let key = format!(" {attr}=\"");
        let k = rest.find(&key).or_else(|| rest.find(&format!("\t{attr}=\"")))? + key.len();
        return rest[k..].split('"').next().map(str::to_string);
    }
    None
}

/// `GR_Dirt_8X1_DIFF.xds` → (8, 1).
/// `FH1_PARTICLE_SHAPE=0`: square, upright quads for every effect (before 2026-10-07).
fn aspect_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_PARTICLE_SHAPE").map_or(true, |v| v != "0"))
}

fn atlas_frames(texture: &str) -> (u8, u8) {
    let up = texture.to_ascii_uppercase();
    for part in up.split(['_', '.']) {
        if let Some((c, r)) = part.split_once('X') {
            if let (Ok(c), Ok(r)) = (c.parse::<u8>(), r.parse::<u8>()) {
                if c > 0 && r > 0 {
                    return (c, r);
                }
            }
        }
    }
    (1, 1)
}

/// A spawn request: `count` particles at `pos`, launched in a cone of the effect's spread around `axis` at its speed,
/// plus `inherit` (added as is: pre-scale it by [`EffectDef::inherit`]).
#[derive(Clone, Copy, Debug)]
pub struct Spawn {
    pub pos: Vec3,
    pub axis: Vec3,
    pub inherit: Vec3,
    pub count: u32,
    /// 0..1: alpha × intensity, size × (0.6 + 0.4 intensity).
    pub intensity: f32,
    /// Size multiplier (1 = the XML).
    pub size_scale: f32,
    /// Soft fade within [`SOFT_HEIGHT`] above this world height; `f32::NEG_INFINITY` = none.
    pub ground_y: f32,
    /// Atlas column; None = random.
    pub frame: Option<u8>,
    /// How far the emitter moved this frame: particles start spread along `pos - sweep × [0, 1)`, so a fast emitter
    /// leaves a continuous trail instead of one clump per frame. Zero = all at `pos`.
    pub sweep: Vec3,
}

impl Spawn {
    pub fn new(pos: Vec3, axis: Vec3, count: u32) -> Self {
        Self { pos, axis, inherit: Vec3::ZERO, count, intensity: 1.0, size_scale: 1.0, ground_y: f32::NEG_INFINITY, frame: None, sweep: Vec3::ZERO }
    }
}

#[derive(Clone, Copy)]
struct Particle {
    pos: Vec3,
    vel: Vec3,
    /// Emission direction (unit; without inherited motion), for `align_travel`.
    axis: Vec3,
    age: f32,
    inv_life: f32,
    size0: f32,
    size_max: f32,
    rot: f32,
    spin: f32,
    alpha: f32,
    ground_y: f32,
    frame: u8,
}

struct Effect {
    def: EffectDef,
    cap: usize,
    parts: Vec<Particle>,
    /// (−view distance², index), reused.
    order: Vec<(f32, u32)>,
    /// Vertex scratch, reused.
    pos: Vec<[f32; 3]>,
    corner: Vec<[f32; 2]>,
    size_rot: Vec<[f32; 2]>,
    colour: Vec<[f32; 4]>,
    extra: Vec<[f32; 4]>,
    draw: Option<(Entity, Handle<Mesh>, Handle<ParticleMaterial>)>,
    /// The material has a gradient texture (`ParticleParams::flags.z`).
    gradient: bool,
    /// Quads in the mesh last frame (0 = hidden).
    drawn: usize,
    /// Drawn by the half-res effects pass ([`goes_half_res`]; fixed per effect) instead of the mesh.
    half: bool,
    /// Half-res path: (texture, gradient), loaded on first draw.
    textures: Option<(Handle<Image>, Option<Handle<Image>>)>,
}

#[derive(Resource)]
pub struct FxParticles {
    enabled: bool,
    root: Option<PathBuf>,
    effects: Vec<Effect>,
    by_name: HashMap<String, Option<EffectId>>,
    live_total: usize,
    rng: u32,
    /// (µs, frames) since the last stats line.
    cpu: (f64, u32),
    /// particles.wgsl for the half-res pass (the same embedded asset the Material uses).
    half_shader: Option<Handle<Shader>>,
    /// P18: effects whose textures `draw` loads ahead of their first spawn, one per frame ([`FxParticles::preload`]).
    warm: Vec<EffectId>,
}

impl FxParticles {
    fn new() -> Self {
        Self {
            enabled: std::env::var("FH1_PARTICLES").map_or(true, |v| v != "0"),
            root: None,
            effects: Vec::new(),
            by_name: HashMap::new(),
            live_total: 0,
            rng: 0x9E37_79B9,
            cpu: (0.0, 0),
            half_shader: None,
            warm: Vec::new(),
        }
    }

    /// P18 (docs/PERF.md; `FH1_FX_PRELOAD=0` = off): resolves these effects now (their XML is read here) and has `draw` load
    /// their textures over the next frames, one effect per frame, before anything spawns them. The race-finish cannons
    /// read 4 XMLs and decoded their DDS synchronously on the finish frame (part of the 1.3 s stall in user log
    /// 20261009_143733). Call it where a short hitch doesn't matter (race load / grid). Unknown names are ignored.
    pub fn preload(&mut self, names: &[&str]) {
        if !preload_on() {
            return;
        }
        for name in names {
            if let Some(id) = self.effect(name) {
                if !self.warm.contains(&id) {
                    self.warm.push(id);
                }
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The effect from `<assets>/effects/<name>.xml` (effects.zip stem: "Smoke", "Dirt1", "Leaf2", ...). None until the
    /// data root is known (first PostUpdate), when the file is missing, or with FH1_PARTICLES=0. Failures are cached.
    pub fn effect(&mut self, name: &str) -> Option<EffectId> {
        if !self.enabled {
            return None;
        }
        let root = self.root.clone()?;
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = std::fs::read_to_string(root.join(format!("{name}.xml"))).ok().map(|xml| {
            let def = EffectDef::parse(name, &xml);
            let cap = (def.budget.max(1) as usize * EMITTERS_PER_EFFECT).min(MAX_PER_EFFECT);
            let half = goes_half_res(&def);
            self.effects.push(Effect {
                half,
                textures: None,
                def,
                cap,
                parts: Vec::with_capacity(cap),
                order: Vec::with_capacity(cap),
                pos: Vec::with_capacity(cap * 4),
                corner: Vec::with_capacity(cap * 4),
                size_rot: Vec::with_capacity(cap * 4),
                colour: Vec::with_capacity(cap * 4),
                extra: Vec::with_capacity(cap * 4),
                draw: None,
                gradient: false,
                drawn: 0,
            });
            EffectId(self.effects.len() as u16 - 1)
        });
        if id.is_none() {
            warn!("fh1-render particles: {} missing (run fh1setup effects)", root.join(format!("{name}.xml")).display());
        }
        self.by_name.insert(name.to_string(), id);
        id
    }

    pub fn def(&self, id: EffectId) -> &EffectDef {
        &self.effects[id.0 as usize].def
    }

    pub fn live(&self, id: EffectId) -> usize {
        self.effects.get(id.0 as usize).map_or(0, |e| e.parts.len())
    }

    /// Particles to start this frame: `Quantity × intensity × dt` × the quality preset's particle scale (P17-A), with the
    /// fraction carried in `carry`.
    pub fn count(&self, id: EffectId, intensity: f32, dt: f32, carry: &mut f32) -> u32 {
        let want = *carry + self.def(id).rate * intensity.max(0.0) * dt * crate::quality::particles();
        let n = want.floor();
        *carry = (want - n).min(1.0);
        n as u32
    }

    fn rand(&mut self) -> f32 {
        // xorshift32
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// −1..1
    fn rand_s(&mut self) -> f32 {
        self.rand() * 2.0 - 1.0
    }

    pub fn spawn(&mut self, id: EffectId, s: &Spawn) {
        let Some(e) = self.effects.get(id.0 as usize) else { return };
        let room = (e.cap - e.parts.len()).min(MAX_LIVE_TOTAL.saturating_sub(self.live_total));
        let n = (s.count as usize).min(room);
        if n == 0 || !self.enabled {
            return;
        }
        let d = e.def.clone_params();
        let axis = s.axis.normalize_or(Vec3::Y);
        let (t1, t2) = axis.any_orthonormal_pair();
        let k = s.intensity.clamp(0.0, 1.0);
        let scale = s.size_scale * (0.6 + 0.4 * k);
        let old = spread_in_radians();
        for _ in 0..n {
            let dir = if old {
                // Old: uniform direction in the cone of half-angle `spread` around the axis.
                let cos = 1.0 - self.rand() * (1.0 - d.spread.cos());
                let sin = (1.0 - cos * cos).max(0.0).sqrt();
                let phi = self.rand() * std::f32::consts::TAU;
                axis * cos + (t1 * phi.cos() + t2 * phi.sin()) * sin
            } else {
                // SuperSpray: tilt = OffAxis value ± variation away from the axis, turned around it by OffPlane value ±
                // variation (random when there is no mean tilt or no plane spread: there's no emitter X axis to
                // measure it from).
                let tilt = d.off_axis + d.spread * self.rand_s();
                let phi = if d.off_axis.abs() > 1e-4 && d.off_plane_var > 0.0 && d.off_plane_var < std::f32::consts::PI {
                    d.off_plane + d.off_plane_var * self.rand_s()
                } else {
                    self.rand() * std::f32::consts::TAU
                };
                axis * tilt.cos() + (t1 * phi.cos() + t2 * phi.sin()) * tilt.sin()
            };
            let speed = d.speed * (1.0 + d.speed_var * self.rand_s());
            let life = d.life * (1.0 + d.life_var * self.rand_s()).max(0.1);
            let size_k = (1.0 + d.size_var * self.rand_s()).max(0.1) * scale;
            let spin = if d.spin_time > 0.0 {
                let t = d.spin_time * (1.0 + d.spin_time_var * self.rand_s()).max(0.1);
                let sign = if self.rand() < 0.5 { -1.0 } else { 1.0 };
                sign * std::f32::consts::TAU / t
            } else {
                0.0
            };
            let rot = d.spin_phase_var * self.rand_s();
            let total = (d.frames.0 as u16 * d.frames.1 as u16).clamp(1, 255) as u8;
            let frame = s.frame.unwrap_or((self.rand() * total as f32) as u8).min(total - 1);
            let p = Particle {
                pos: s.pos - s.sweep * self.rand(),
                vel: dir * speed + s.inherit,
                axis: dir,
                age: 0.0,
                inv_life: 1.0 / life,
                size0: d.size * size_k,
                size_max: d.size_max * size_k,
                rot,
                spin,
                alpha: k,
                ground_y: s.ground_y,
                frame,
            };
            self.effects[id.0 as usize].parts.push(p);
        }
        self.live_total += n;
    }
}

/// The spawn-time numbers of an effect, copied out so `spawn` can borrow the RNG.
#[derive(Clone, Copy)]
struct SpawnParams {
    speed: f32,
    speed_var: f32,
    spread: f32,
    off_axis: f32,
    off_plane: f32,
    off_plane_var: f32,
    life: f32,
    life_var: f32,
    size: f32,
    size_var: f32,
    size_max: f32,
    spin_time: f32,
    spin_time_var: f32,
    spin_phase_var: f32,
    frames: (u8, u8),
}

impl EffectDef {
    fn clone_params(&self) -> SpawnParams {
        SpawnParams {
            speed: self.speed,
            speed_var: self.speed_var,
            spread: self.spread,
            off_axis: self.off_axis,
            off_plane: self.off_plane,
            off_plane_var: self.off_plane_var,
            life: self.life,
            life_var: self.life_var,
            size: self.size,
            size_var: self.size_var,
            size_max: self.size_max,
            spin_time: self.spin_time,
            spin_time_var: self.spin_time_var,
            spin_phase_var: self.spin_phase_var,
            frames: self.frames,
        }
    }
}

/// The data root: `<assets>/effects`, next to the post chain's `shaders/xex`.
fn find_root(mut fx: ResMut<FxParticles>, config: Option<Res<crate::postfx::FxPostConfig>>) {
    if fx.root.is_some() || !fx.enabled {
        return;
    }
    if let Some(root) = config.and_then(|c| c.0.xex_dir.parent().and_then(|p| p.parent()).map(|a| a.join("effects"))) {
        fx.root = Some(root);
    }
}

/// `FH1_PARTICLES_STATS=1`: every 2 s, live particles and the mean CPU time of simulate + draw (µs/frame).
fn stats(mut fx: ResMut<FxParticles>, time: Res<Time<Real>>, mut last: Local<f64>) {
    if !*STATS.get_or_init(|| std::env::var("FH1_PARTICLES_STATS").is_ok_and(|v| v == "1")) {
        return;
    }
    let now = time.elapsed_secs_f64();
    if now - *last >= 2.0 {
        *last = now;
        let (us, frames) = fx.cpu;
        let live: Vec<String> = fx.effects.iter().filter(|e| !e.parts.is_empty()).map(|e| format!("{} {}", e.def.name, e.parts.len())).collect();
        info!("particles: {} live {live:?}, cpu {:.0} us/frame", fx.live_total, if frames > 0 { us / frames as f64 } else { 0.0 });
        fx.cpu = (0.0, 0);
    }
}

static STATS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn simulate(time: Res<Time>, mut fx: ResMut<FxParticles>) {
    let t0 = std::time::Instant::now();
    let dt = time.delta_secs().min(0.1);
    if dt <= 0.0 {
        return;
    }
    let mut live = 0;
    for e in &mut fx.effects {
        let d = &e.def;
        let (g, drag) = (d.gravity, (-d.friction * dt).exp());
        e.parts.retain_mut(|p| {
            p.age += dt * p.inv_life;
            if p.age >= 1.0 {
                return false;
            }
            p.vel.y -= g * dt;
            p.vel *= drag;
            p.pos += p.vel * dt;
            p.rot += p.spin * dt;
            true
        });
        live += e.parts.len();
    }
    fx.live_total = live;
    fx.cpu.0 += t0.elapsed().as_secs_f64() * 1e6;
    fx.cpu.1 += 1;
}

/// How much of the scene light a particle gets: `ambColor × li.z + sunColor × li.y × 0.25` (linear; INFERRED from
/// Smoke.xml light_influence (0, 2, 2.7, 1) so white smoke at 16:00 sits near the lit road's brightness; the game's
/// CPU lighting code is not traced). Effects without dynamic_lighting are unlit (colour as stored).
fn light_scale(d: &EffectDef, amb: Vec3, sun: Vec3) -> Vec3 {
    if !d.dynamic_lighting {
        return Vec3::ONE;
    }
    (amb * d.light_influence.z + sun * d.light_influence.y * 0.25) * light_gain()
}

#[allow(clippy::too_many_arguments)]
fn draw(
    mut commands: Commands,
    mut fx: ResMut<FxParticles>,
    globals: Option<Res<crate::FxGlobals>>,
    lib: Option<Res<crate::FxLibrary>>,
    cams: Query<(&GlobalTransform, &Camera), With<crate::post::FxPostCamera>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ParticleMaterial>>,
    mut vis: Query<(&mut Visibility, &mut Transform, &mut GlobalTransform), Without<crate::post::FxPostCamera>>,
    (assets, mut half_res): (Res<AssetServer>, Option<ResMut<crate::fx_half_res::FxHalfRes>>),
) {
    let Some(root) = fx.root.clone() else { return };
    let t0 = std::time::Instant::now();
    // Half-res effects (fx_half_res.rs): the effect's own shader, compiled there with FX_HALF_RES.
    let half_shader = if crate::fx_half_res::on() && half_res.is_some() {
        Some(fx.half_shader.get_or_insert_with(|| assets.load("embedded://fh1_render/particles.wgsl")).clone())
    } else {
        None
    };
    let cam = cams.iter().find(|c| c.1.is_active).map_or(Vec3::ZERO, |c| c.0.translation());
    let cam_axes = cams.iter().find(|c| c.1.is_active).map_or((Vec3::X, Vec3::Y), |c| (c.0.right().as_vec3(), c.0.up().as_vec3()));
    let g = |n: &str| globals.as_ref().and_then(|g| g.get(n)).unwrap_or(Vec4::ZERO);
    let (amb, sun) = (g("ambColor").truncate(), g("sunColor").truncate());
    let raw = lib.as_ref().is_some_and(|l| l.raw_output);
    let params = ParticleParams {
        fog: g("FogConsts"),
        fog_colour: g("FogColor"),
        fog2: g("FogConsts2"),
        fog_colour2: g("FogColor2"),
        sun_dir: g("sunDir"),
        flags: Vec4::new(raw as u32 as f32, SOFT_HEIGHT, 0.0, crate::output_gain()),
        shape: Vec4::ZERO,
    };
    let lag_fix = lag_fix();
    // P18: one preloaded effect's textures per frame (FxParticles::preload).
    if let Some(id) = fx.warm.pop() {
        if let Some(e) = fx.effects.get_mut(id.0 as usize) {
            if e.textures.is_none() {
                let d = &e.def;
                if let Some(texture) = load_texture(&mut images, &root, &d.texture, ImageAddressMode::ClampToEdge) {
                    let gradient = (!d.gradient.is_empty()).then(|| load_texture(&mut images, &root, &d.gradient, ImageAddressMode::ClampToEdge)).flatten();
                    e.gradient = gradient.is_some();
                    e.textures = Some((texture, gradient));
                }
            }
        }
    }
    for e in &mut fx.effects {
        let n = e.parts.len();
        if n == 0 {
            if e.drawn > 0 {
                if let Some((ent, ..)) = e.draw {
                    if let Ok((mut v, ..)) = vis.get_mut(ent) {
                        v.set_if_neq(Visibility::Hidden);
                    }
                }
                e.drawn = 0;
            }
            continue;
        }
        // Back to front; the entity sits at the centroid so Bevy's transparent sort places the whole batch sensibly.
        e.order.clear();
        let mut centroid = Vec3::ZERO;
        for (i, p) in e.parts.iter().enumerate() {
            e.order.push((-(p.pos - cam).length_squared(), i as u32));
            centroid += p.pos;
        }
        centroid /= n as f32;
        e.order.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let d = &e.def;
        // Per-effect uniform: the gradient flag (the shared globals used to overwrite it on every update) and the
        // screen-size cap.
        let mut params = params;
        if lag_fix {
            params.flags.z = e.gradient as u32 as f32;
        }
        params.shape.x = if max_screen_on() { d.max_screen } else { 0.0 };
        // Encoded for the gamma-2 buffer: sqrt of the linear light; the XML colour stays as stored (the game writes
        // tex × colour straight into it).
        let light = light_scale(d, amb, sun).max(Vec3::ZERO).map(f32::sqrt);
        let fw = 1.0 / d.frames.0.max(1) as f32;
        let cols = d.frames.0.max(1);
        params.shape.y = d.aspect;
        params.shape.z = 1.0 / d.frames.1.max(1) as f32;
        let (cam_right, cam_up) = cam_axes;
        // Half-res path: world-space quads into one FxBatch, no mesh (the effect never gets one; a mesh left from a
        // fallback frame is hidden).
        let half_shader = half_shader.as_ref().filter(|_| e.half);
        if half_shader.is_some() {
            if let Some((ent, ..)) = e.draw {
                if let Ok((mut v, ..)) = vis.get_mut(ent) {
                    v.set_if_neq(Visibility::Hidden);
                }
            }
            e.drawn = 0;
            if e.textures.is_none() {
                let texture = load_texture(&mut images, &root, &d.texture, ImageAddressMode::ClampToEdge);
                let gradient = (!d.gradient.is_empty()).then(|| load_texture(&mut images, &root, &d.gradient, ImageAddressMode::ClampToEdge)).flatten();
                let Some(texture) = texture else {
                    warn!("fh1-render particles: texture {} for {} missing; effect not drawn", d.texture, d.name);
                    e.parts.clear();
                    continue;
                };
                e.gradient = gradient.is_some();
                e.textures = Some((texture, gradient));
            }
        }
        let mut half_verts: Vec<crate::fx_half_res::FxVertex> = if half_shader.is_some() { Vec::with_capacity(n * 4) } else { Vec::new() };
        e.pos.clear();
        e.corner.clear();
        e.size_rot.clear();
        e.colour.clear();
        e.extra.clear();
        for &(_, i) in &e.order {
            let p = &e.parts[i as usize];
            let c = d.colour_at(p.age);
            let half = 0.5 * d.size_at(p.age, p.size0, p.size_max);
            let rgb = c.truncate() * light;
            let colour = [rgb.x, rgb.y, rgb.z, c.w * p.alpha];
            // Row packed into the integer part of the atlas u offset (the shader splits it).
            let (col, row) = (p.frame % cols, p.frame / cols);
            let extra = [p.ground_y.max(-1.0e30), row as f32 + col as f32 * fw, fw, p.age];
            let (mut centre, mut rot) = (p.pos, p.rot);
            if d.align_travel {
                // Up = the emission direction on screen; the quad's base (texture bottom) at the particle.
                let (dx, dy) = (p.axis.dot(cam_right), p.axis.dot(cam_up));
                let l = (dx * dx + dy * dy).sqrt();
                if l > 1e-3 {
                    rot = (-dx).atan2(dy);
                    centre += (cam_right * dx + cam_up * dy) / l * (half * d.aspect);
                }
            }
            if half_shader.is_some() {
                let pos = centre.to_array();
                for corner in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
                    half_verts.push(crate::fx_half_res::FxVertex { pos, corner, size_rot: [half, rot], colour, extra });
                }
                continue;
            }
            let local = (centre - centroid).to_array();
            for corner in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
                e.pos.push(local);
                e.corner.push(corner);
                e.size_rot.push([half, rot]);
                e.colour.push(colour);
                e.extra.push(extra);
            }
        }
        if let (Some(shader), Some(half_res), Some((texture, gradient))) = (half_shader, half_res.as_deref_mut(), e.textures.as_ref()) {
            params.flags.z = e.gradient as u32 as f32;
            half_res.push(crate::fx_half_res::FxBatch {
                shader: shader.clone(),
                blend: if d.blend == Blend::Add { crate::fx_half_res::FxBlend::Add } else { crate::fx_half_res::FxBlend::Alpha },
                tex0: Some(texture.clone()),
                tex1: gradient.clone(),
                params: crate::fx_half_res::FxBatch::uniform(&params),
                verts: half_verts,
                dist2: centroid.distance_squared(cam),
            });
            continue;
        }
        match &e.draw {
            Some((ent, mesh, mat)) => {
                if let Some(mut m) = meshes.get_mut(mesh) {
                    fill_mesh(&mut m, e, n);
                }
                if let Some(m) = materials.get(mat) {
                    if m.params != params {
                        if let Some(mut m) = materials.get_mut(mat) {
                            m.params = params;
                        }
                    }
                }
                if let Ok((mut v, mut t, mut gt)) = vis.get_mut(*ent) {
                    v.set_if_neq(Visibility::Inherited);
                    t.translation = centroid;
                    // This runs after transform propagation: write the GlobalTransform the renderer extracts this
                    // frame, or the vertices (relative to this frame's centroid) draw around last frame's.
                    if lag_fix {
                        *gt = GlobalTransform::from_translation(centroid);
                    }
                }
            }
            None => {
                // Preloaded (P18) or loaded by the half-res path before: reuse, else load now.
                let (texture, gradient) = match e.textures.clone() {
                    Some((t, g)) => (Some(t), g),
                    None => (
                        load_texture(&mut images, &root, &d.texture, ImageAddressMode::ClampToEdge),
                        (!d.gradient.is_empty()).then(|| load_texture(&mut images, &root, &d.gradient, ImageAddressMode::ClampToEdge)).flatten(),
                    ),
                };
                let Some(texture) = texture else {
                    warn!("fh1-render particles: texture {} for {} missing; effect not drawn", d.texture, d.name);
                    e.parts.clear();
                    continue;
                };
                let mut p = params;
                p.flags.z = gradient.is_some() as u32 as f32;
                e.gradient = gradient.is_some();
                let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
                m.insert_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::with_capacity(e.cap * 4));
                m.insert_attribute(Mesh::ATTRIBUTE_UV_0, Vec::<[f32; 2]>::with_capacity(e.cap * 4));
                m.insert_attribute(Mesh::ATTRIBUTE_UV_1, Vec::<[f32; 2]>::with_capacity(e.cap * 4));
                m.insert_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::with_capacity(e.cap * 4));
                m.insert_attribute(Mesh::ATTRIBUTE_TANGENT, Vec::<[f32; 4]>::with_capacity(e.cap * 4));
                m.insert_indices(Indices::U32(Vec::with_capacity(e.cap * 6)));
                fill_mesh(&mut m, e, n);
                let mesh = meshes.add(m);
                let mat = materials.add(ParticleMaterial { texture, gradient, params: p, additive: d.blend == Blend::Add });
                let ent = commands
                    .spawn((
                        Mesh3d(mesh.clone()),
                        MeshMaterial3d(mat.clone()),
                        Transform::from_translation(centroid),
                        GlobalTransform::from_translation(centroid),
                        Visibility::Inherited,
                        NoFrustumCulling,
                        bevy::light::NotShadowCaster,
                        bevy::light::NotShadowReceiver,
                        Name::new(format!("FH1 particles {}", d.name)),
                    ))
                    .id();
                e.draw = Some((ent, mesh, mat));
            }
        }
        e.drawn = n;
    }
    fx.cpu.0 += t0.elapsed().as_secs_f64() * 1e6;
}

/// P18: [`FxParticles::preload`] on (`FH1_FX_PRELOAD=0` = effects load on their first spawn, as before).
fn preload_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_FX_PRELOAD").map_or(true, |v| v != "0"))
}

/// Stable GPU allocations for the per-frame quad meshes (these particles, engine smoke.rs / backfire.rs). P13
/// (2026-10-08; `FH1_FX_MESH_CAP=0` = old, exact-size meshes). Every modified mesh is freed and re-allocated by Bevy's
/// mesh allocator in the frame it is extracted; with the exact live count its size changed every frame, so the range
/// rarely fitted the hole it left, fragmenting the shared u32 index slab (and the effect's vertex slab) until a slab had
/// to grow (new buffer + copy of the whole slab: a hitch). Now a quad mesh is padded to a power-of-two quad capacity
/// (from 64; grown at once, shrunk only below a quarter): the size changes only on a bucket step, so each frame's range
/// reuses the one just freed. Padding = zeroed vertices that nothing indexes + degenerate (0, 0, 0) triangles, which
/// rasterise nothing whatever the shader does.
pub fn fx_mesh_cap_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_FX_MESH_CAP").map_or(true, |v| v != "0"))
}

/// Quad capacity for `n` live quads given the mesh's current capacity `cur` ([`fx_mesh_cap_on`]).
pub fn quad_capacity(n: usize, cur: usize) -> usize {
    let want = n.max(1).next_power_of_two().max(64);
    if want > cur || want.saturating_mul(4) <= cur {
        want
    } else {
        cur
    }
}

/// Rewrites a quad mesh's u32 indices for `n` quads (quad q = vertices 4q..4q+3, two triangles), keeping the leading
/// quads already there. A tail padded by [`pad_quad_mesh`] (degenerate zeros) is not mistaken for quads: the real
/// prefix is found by binary search (quad q's second index is 4q + 1, a padded one's is 0).
pub fn quad_indices(idx: &mut Vec<u32>, n: usize) {
    let have = idx.len() / 6;
    let (mut lo, mut hi) = (0usize, have);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if idx[mid * 6 + 1] == mid as u32 * 4 + 1 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    idx.truncate(lo.min(n) * 6);
    for q in idx.len() / 6..n {
        let b = q as u32 * 4;
        idx.extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
    }
}

/// Current quad capacity of a quad mesh (index count / 6).
pub fn quad_mesh_capacity(m: &Mesh) -> usize {
    m.indices().map_or(0, |i| i.len() / 6)
}

/// Pads a quad mesh holding `n` quads (4 vertices, 6 u32 indices each) to the capacity chosen from `prev` (the
/// capacity before this frame's fill), see [`fx_mesh_cap_on`]. No-op with the flag off.
pub fn pad_quad_mesh(m: &mut Mesh, n: usize, prev: usize) {
    if !fx_mesh_cap_on() {
        return;
    }
    let cap = quad_capacity(n, prev);
    let verts = cap * 4;
    for (_, values) in m.attributes_mut() {
        match values {
            VertexAttributeValues::Float32x2(v) if v.len() < verts => v.resize(verts, [0.0; 2]),
            VertexAttributeValues::Float32x3(v) if v.len() < verts => v.resize(verts, [0.0; 3]),
            VertexAttributeValues::Float32x4(v) if v.len() < verts => v.resize(verts, [0.0; 4]),
            _ => {}
        }
    }
    if let Some(Indices::U32(idx)) = m.indices_mut() {
        if idx.len() < cap * 6 {
            idx.resize(cap * 6, 0);
        }
    }
}

/// Copy the effect's vertex scratch into the mesh's own arrays (capacity kept, so no allocation after warm-up).
fn fill_mesh(m: &mut Mesh, e: &Effect, n: usize) {
    fn put<T: Copy>(dst: &mut Vec<T>, src: &[T]) {
        dst.clear();
        dst.extend_from_slice(src);
    }
    let prev = quad_mesh_capacity(m);
    if let Some(VertexAttributeValues::Float32x3(v)) = m.attribute_mut(Mesh::ATTRIBUTE_POSITION) {
        put(v, &e.pos);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        put(v, &e.corner);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_1) {
        put(v, &e.size_rot);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_COLOR) {
        put(v, &e.colour);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_TANGENT) {
        put(v, &e.extra);
    }
    if let Some(Indices::U32(idx)) = m.indices_mut() {
        quad_indices(idx, n);
    }
    pad_quad_mesh(m, n, prev);
}

/// `<root>/<stem>.dds` (fh1setup effects: RGBA8 with mips, the .xds texel values as stored).
fn load_texture(images: &mut Assets<Image>, root: &std::path::Path, xds: &str, address: ImageAddressMode) -> Option<Handle<Image>> {
    let stem = xds.rsplit_once('.').map_or(xds, |s| s.0);
    let mut img = crate::scenery::read_dds(&root.join(format!("{stem}.dds")))?;
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: address,
        address_mode_v: address,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..default()
    });
    Some(images.add(img))
}

/// `particles.wgsl` `ParticleParams`.
#[derive(Clone, Copy, ShaderType, Debug, PartialEq)]
pub struct ParticleParams {
    pub fog: Vec4,
    pub fog_colour: Vec4,
    pub fog2: Vec4,
    pub fog_colour2: Vec4,
    pub sun_dir: Vec4,
    /// x = 1: raw output (FH1 post chain, sqrt-encoded colour), y = soft fade height (m), z = 1: gradient texture.
    pub flags: Vec4,
    /// x = max_screen_size (fraction of the viewport height, 0 = no cap).
    pub shape: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
#[bind_group_data(ParticleKey)]
pub struct ParticleMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    #[texture(3)]
    #[sampler(4)]
    pub gradient: Option<Handle<Image>>,
    #[uniform(2)]
    pub params: ParticleParams,
    pub additive: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParticleKey {
    additive: bool,
}

impl From<&ParticleMaterial> for ParticleKey {
    fn from(m: &ParticleMaterial) -> Self {
        Self { additive: m.additive }
    }
}

impl Material for ParticleMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_render/particles.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_render/particles.wgsl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        if self.additive { AlphaMode::Add } else { AlphaMode::Blend }
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, key: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        d.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(2),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(3),
            Mesh::ATTRIBUTE_TANGENT.at_shader_location(4),
        ])?];
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                let (src, dst) = if key.bind_group_data.additive { (BlendFactor::SrcAlpha, BlendFactor::One) } else { (BlendFactor::SrcAlpha, BlendFactor::OneMinusSrcAlpha) };
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: src, dst_factor: dst, operation: BlendOperation::Add },
                    alpha: BlendComponent { src_factor: BlendFactor::Zero, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                });
            }
        }
        if let Some(ds) = d.depth_stencil.as_mut() {
            ds.depth_write_enabled = Some(false).into();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMOKE: &str = r#"<Effect Version="13" budget="180" lowerbudget="100">
  <EmitterProperties type="SuperSpray">
    <OffAxis value="0.000000" variation="1.718873"/>
    <Quantity value="20.000000" var="0.000000"/>
    <Velocity value="0.849996" variation="0.250000"/>
    <Life value="1.600001" variation="0.000000"/>
    <Size valueX="0.140000" valueY="0.140000" variationX="0.350000" variationY="0.350000" growforX="1.000000" growforY="1.000000" shrinkforX="0.000000" shrinkforY="0.000000" maxValueX="1.000000" maxValueY="1.000000" endValueX="0.000000" endValueY="0.000000" growFrom="0.000000" shrinkTo="0.000000"/>
    <SpinTime value="5.000000" variation="0.400000"/>
  </EmitterProperties>
  <Behavior>
    <MotionInheritance influence="100" multiplier="0.450000" offset="0.000000" variation="0.000000"/>
    <Gravity strength="-0.500000" decay="0.000000"/>
    <Friction strength="0.650001" startFrom="0.000000" velocityKept="3.200000"/>
  </Behavior>
  <Render>
    <ParticleType value="BillboardedQuad" dynamic_lighting="1" zsort="0">
      <light_influence v.x="0.000000" v.y="1.999999" v.z="2.699999" v.w="1.000000"/>
      <Texture value="GR_Puffy_Smoke_DIFF.xds"/>
      <Texture2 value=""/>
      <Material>
        <BlendType value="alpha"/>
        <RGBStart r="1.000000" g="1.000000" b="1.000000" a="0.588235"/>
        <RGBEnd r="0.196078" g="0.196078" b="0.196078" a="0.000000"/>
        <FadeInTo fadeInTo="0.150000"/>
        <FadeOutFrom fadeOutFrom="0.560000"/>
        <RGBMid r="1.000000" g="1.000000" b="1.000000" a="0.392157"/>
      </Material>
    </ParticleType>
  </Render>
</Effect>"#;

    #[test]
    fn parses_smoke() {
        let d = EffectDef::parse("Smoke", SMOKE);
        assert_eq!(d.budget, 180);
        assert_eq!(d.rate, 20.0);
        assert!((d.size_max - 1.0).abs() < 1e-6 && (d.size - 0.14).abs() < 1e-6);
        assert!((d.inherit - 0.45).abs() < 1e-6);
        assert!(d.gravity < 0.0 && d.dynamic_lighting && d.blend == Blend::Alpha);
        assert_eq!(d.frames, (1, 1));
        // OffAxis variation 1.718873 is degrees (0.03 rad), not a 98° cone.
        assert!((d.spread - 0.03).abs() < 1e-4);
        assert!((d.colour_at(0.3).w - 0.392157).abs() < 1e-5);
        assert!(d.colour_at(1.0).w.abs() < 1e-6);
        assert_eq!(atlas_frames("GR_Dirt_8X1_DIFF.xds"), (8, 1));
    }
}
