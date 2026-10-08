//! Remaster tyre smoke (docs/EFFECTS.md "Remaster smoke"; user 2026-10-07: "not too bothered about it being 1:1 ... we
//! want it exaggerated ... we want it to look good"). Replaces effects.rs's game-parity smoke while on
//! (`FH1_SMOKE=old` = the effects.zip Smoke/SmokeRim systems, `FH1_SMOKE=0` = none).
//!
//! Not from the game: big, long-lived billowing volumes. Each wheel of every car (player, AI, traffic) emits puffs from
//! its contact patch by tread slip (wheelspin, lock-up, slide; the same tread vector as effects.rs), spaced along the
//! path so a fast slide leaves a continuous wall. Puffs start ~0.6 m, grow to 3-6 m over 4-9 s, keep part of the
//! car's motion then settle into a slow drift + rise with per-puff turbulence, and thin out as they expand.
//!
//! Drawn as one sorted, alpha-blended billboard mesh through smoke.wgsl: two scrolling layers of a tiling fbm noise
//! texture (built here) carve a billowy silhouette and a fake sphere normal, lit by the scene's actual directional light
//! (direction, colour, lux) with wrap diffuse, forward scattering toward the sun, self-shadowed core and sky ambient,
//! scaled by the view exposure (works under the Remaster HDR camera). Soft at the ground and near the camera.
//!
//! Volume (2026-10-08, user: "looks low quality, thick smoke clips through the car body"; default, `FH1_SMOKE_VOLUME=0`
//! = flat quads): each puff is drawn as a sphere: smoke.wgsl clips the view ray's chord through it against the road plane
//! and two boxes per nearby car (lower body + cabin from the PristineBoundingBox, the 4 cars nearest the camera), so the
//! smoke thins smoothly into the body and the road (soft particles without a depth texture), lights it with the sphere's
//! normal and the sun's path through the puff (self-shadow), and adds a finer noise layer (512² texture, 6 octaves).
//! Puffs are pushed out of car hulls while young (`FH1_SMOKE_PUSH=0` = off): the nearest side / end / roof face, with
//! the velocity into the body removed, so smoke pours out of the arches instead of sitting inside the car.
//! [`Smoke::exhaust_puff`] adds a dark-ish exhaust puff (backfire.rs, after a bang).
//!
//! 2026-10-08 (user: "when a tyre skips at speed, a literal perfect circle of smoke comes from it"):
//! - **Sustained slip only** (`FH1_SMOKE_SUSTAIN=0` = old): per wheel, smoke ramps in over 0.15-0.45 s of continuous slip
//!   above the start threshold, which rises with speed (+0.05 m/s of slip per m/s of car speed, `FH1_SMOKE_SPEED_SLIP`);
//!   the timer drains 3x as fast as it fills and resets in the air. A bump / landing skip or a brief lock-up makes none.
//!   OUR rule: the game's per-wheel smoke rule is untraced (docs/EFFECTS.md "Tyre smoke").
//! - **Small, dense start**: puffs are born at 0.25-0.4 m (was 0.5-0.8) and expand.
//! - **No circles** (smoke.wgsl): each puff is 3 overlapping lobes (offsets / sizes from its seed) with a domain-warped,
//!   noise-eroded edge, so no puff outline is round; puffs moving fast relative to the camera stretch along their screen
//!   motion (an ellipsoid of up to 3x, ~60 ms of motion), a smeared trail instead of round blobs (`FH1_SMOKE_STRETCH`,
//!   seconds of motion, 0.06; 0 = none).
//!
//! Burnout whiteout (2026-10-08, race autotest: 25 s of burnout against a barrier turned the whole frame grey;
//! `FH1_SMOKE_BURNOUT=0` = old):
//! - **Camera fade**: a puff fades as the camera nears its centre (full beyond 1.3 r + 1.5 m, none inside 0.5 r) and is
//!   not drawn at all inside 0.5 r: the camera never sits behind dozens of enveloping spheres (whiteout and full-screen
//!   fill), the cloud stays visible around it.
//! - **Standing emission**: per wheel at most 6 puffs/s at a standstill rising to the full ~17/s by 10 m/s
//!   (`FH1_SMOKE_STILL_RATE`).
//! - **Disperse**: puffs from a car slower than 5 m/s rise faster (x1.8), spread wider and live 0.65x as long, so a
//!   standing burnout sends a column up and away instead of pooling round the car.
//! - **Live cap** `FH1_SMOKE_MAX` 1200 (was 3000; the oldest is recycled).
//!
//! Knobs: `FH1_SMOKE_DEBUG=1` (every wheel smokes), `FH1_SMOKE_AMOUNT` (emission, 1.0), `FH1_SMOKE_OPACITY` (1.0), `FH1_SMOKE_SIZE` (1.0), `FH1_SMOKE_LIFE` (1.0), `FH1_SMOKE_MAX` (particles,
//! 3000).

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexAttributeValues};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, Extent3d, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
    TextureDimension, TextureFormat,
};
use bevy::shader::ShaderRef;
use fh1_engine::ai::AiCar;
use fh1_engine::vehicle::Vehicle;

use crate::track::Track;
use crate::Car;

pub struct SmokePlugin;

impl Plugin for SmokePlugin {
    fn build(&self, app: &mut App) {
        if mode() != Mode::New {
            return;
        }
        embedded_asset!(app, "smoke.wgsl");
        app.add_plugins(MaterialPlugin::<SmokeMaterial>::default())
            .init_resource::<Smoke>()
            .add_systems(Update, emit.run_if(crate::ui::driving))
            .add_systems(PostUpdate, draw.after(bevy::transform::TransformSystems::Propagate));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Old,
    New,
}

/// `FH1_SMOKE`: unset = New, `old` = effects.rs game smoke, `0` = none.
pub fn mode() -> Mode {
    static M: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("FH1_SMOKE").unwrap_or_default().to_ascii_lowercase().as_str() {
        "0" | "off" => Mode::Off,
        "old" => Mode::Old,
        _ => Mode::New,
    })
}

fn debug() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("FH1_SMOKE_DEBUG").is_ok_and(|v| v == "1"))
}

fn knob(name: &str, d: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[derive(Clone, Copy)]
struct Puff {
    pos: Vec3,
    vel: Vec3,
    age: f32,
    life: f32,
    size0: f32,
    size1: f32,
    rot: f32,
    spin: f32,
    seed: f32,
    density: f32,
    ground: f32,
    /// Rise / spread multiplier (1 = moving car; > 1 from a standing burnout, module doc).
    lift: f32,
}

fn burnout_fix() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_BURNOUT").map_or(true, |v| v != "0"))
}

#[derive(Resource)]
pub struct Smoke {
    puffs: Vec<Puff>,
    /// Path distance carried per (car, wheel) since the last puff.
    carry: std::collections::HashMap<(Entity, usize), f32>,
    /// Sustained-slip time per (car, wheel) (module doc).
    sustain: std::collections::HashMap<(Entity, usize), f32>,
    /// Camera position last frame (puff motion relative to the camera, for the stretch).
    last_cam: Option<Vec3>,
    /// SkidData/SmokeType@Smoke by surface id (None = smoke everywhere).
    weights: Option<Vec<f32>>,
    rng: u32,
    max: usize,
    amount: f32,
    opacity: f32,
    size: f32,
    life: f32,
    draw: Option<(Entity, Handle<Mesh>, Handle<SmokeMaterial>)>,
    // Scratch.
    order: Vec<(f32, u32)>,
    pos: Vec<[f32; 3]>,
    corner: Vec<[f32; 2]>,
    size_rot: Vec<[f32; 2]>,
    colour: Vec<[f32; 4]>,
    extra: Vec<[f32; 4]>,
}

impl Default for Smoke {
    fn default() -> Self {
        Smoke {
            puffs: Vec::new(),
            carry: Default::default(),
            sustain: Default::default(),
            last_cam: None,
            weights: None,
            rng: 0x2545_F491,
            max: knob("FH1_SMOKE_MAX", if burnout_fix() { 1200.0 } else { 3000.0 }).clamp(100.0, 20000.0) as usize,
            amount: knob("FH1_SMOKE_AMOUNT", 1.0).max(0.0),
            opacity: knob("FH1_SMOKE_OPACITY", 1.0).clamp(0.0, 3.0),
            size: knob("FH1_SMOKE_SIZE", 1.0).max(0.1),
            life: knob("FH1_SMOKE_LIFE", 1.0).max(0.1),
            draw: None,
            order: Vec::new(),
            pos: Vec::new(),
            corner: Vec::new(),
            size_rot: Vec::new(),
            colour: Vec::new(),
            extra: Vec::new(),
        }
    }
}

impl Smoke {
    /// A short exhaust puff after a backfire bang (backfire.rs): small, quick, thinner than tyre smoke. `k` = loudness 0..1.
    pub fn exhaust_puff(&mut self, pos: Vec3, vel: Vec3, k: f32) {
        if self.puffs.len() >= self.max {
            return;
        }
        let k = k.clamp(0.0, 1.0);
        let jitter = Vec3::new(self.rand_s(), self.rand() * 0.5, self.rand_s()) * 0.4;
        let puff = Puff {
            pos,
            vel: vel + jitter,
            age: 0.0,
            life: (1.2 + 0.8 * self.rand()) * (0.7 + 0.5 * k),
            size0: 0.25 + 0.1 * k,
            size1: (0.9 + 0.5 * self.rand()) * (0.8 + 0.6 * k),
            rot: self.rand() * std::f32::consts::TAU,
            spin: self.rand_s() * 0.6,
            seed: self.rand(),
            density: (0.18 + 0.17 * k) * self.opacity,
            ground: pos.y - 0.4,
            lift: 1.0,
        };
        self.puffs.push(puff);
    }

    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
    fn rand_s(&mut self) -> f32 {
        self.rand() * 2.0 - 1.0
    }
}

fn sustain_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_SUSTAIN").map_or(true, |v| v != "0"))
}

/// Puffs/s per wheel at a standstill (module doc).
fn knob_still_rate() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| knob("FH1_SMOKE_STILL_RATE", 6.0).max(0.5))
}

/// Extra start slip (m/s) per m/s of car speed (module doc).
fn knob_speed_slip() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| knob("FH1_SMOKE_SPEED_SLIP", 0.05).max(0.0))
}

/// Seconds of screen motion a puff is stretched over (module doc).
fn stretch_secs() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| knob("FH1_SMOKE_STRETCH", 0.06).max(0.0))
}

/// Stricter start (2026-10-08, user: "squeal round a corner doesn't have to smoke; a split-second traction break throws a
/// puff"): the slip must also be large for the car's speed (tread slip / max(speed, 5 m/s) above `FH1_SMOKE_SLIP_RATIO`,
/// 0.22 ~ 13 deg, past the tyre's peak, i.e. sliding rather than squealing; full at +0.2), only that counts toward the
/// sustain timer, and smoke ramps in over 0.3-0.7 s of it (was 0.15-0.45). `FH1_SMOKE_STRICT=0` = old.
fn strict_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_STRICT").map_or(true, |v| v != "0"))
}

fn knob_slip_ratio() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| knob("FH1_SMOKE_SLIP_RATIO", 0.22).max(0.0))
}

/// Break-up (2026-10-08, user: "at speed when drifting you can see uniform shapes coming off the wheels"): uneven puff
/// spacing, a slow per-wheel gust on rate and density, mixed puff sizes (wisps to clumps) and densities, emission points
/// scattered over the tread, per-puff stretch. `FH1_SMOKE_BREAKUP=0` = old (even stream).
fn breakup_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_BREAKUP").map_or(true, |v| v != "0"))
}

/// Tread slip (m/s) where smoke starts and the slip above that for full smoke.
const SLIP_START: f32 = 1.5;
const SLIP_FULL: f32 = 6.0;
/// Path spacing of puffs at full smoke (m); thinner smoke spaces them further apart.
const SPACING: f32 = 0.3;

fn emit(mut smoke: ResMut<Smoke>, track: Res<Track>, cars: Query<(Entity, Option<&Car>, Option<&AiCar>)>, fixed: Res<Time<Fixed>>, time: Res<Time>) {
    let smoke = &mut *smoke;
    let dt = time.delta_secs().min(0.1);
    if smoke.weights.is_none() {
        smoke.weights = Some(track.world.as_ref().map_or_else(Vec::new, |w| w.world.surfaces.iter().map(|s| s.param("SkidData/SmokeType@Smoke").unwrap_or(0.0)).collect()));
    }
    let alpha = fixed.overstep_fraction();
    // Simulate: drag toward a slow wind drift, rise, turbulence.
    let wind = Vec3::new(0.5, 0.0, 0.25);
    let t = time.elapsed_secs();
    smoke.puffs.retain_mut(|p| {
        p.age += dt;
        if p.age >= p.life {
            return false;
        }
        let u = p.age / p.life;
        let target = wind * p.lift + Vec3::Y * (0.35 + 0.25 * u) * p.lift;
        let k = 1.0 - (-dt / 0.7).exp();
        p.vel += (target - p.vel) * k;
        let ph = p.seed * 37.0;
        let turb = Vec3::new((t * 0.9 + ph).sin(), (t * 0.7 + ph * 1.3).sin() * 0.5, (t * 0.8 + ph * 0.7).cos()) * 0.35;
        p.pos += (p.vel + turb) * dt;
        p.pos.y = p.pos.y.max(p.ground + 0.15);
        p.rot += p.spin * dt;
        true
    });

    if push_on() && !smoke.puffs.is_empty() {
        // Young puffs out of every car's hull (module doc): to the nearest side / end / roof face, minus the velocity
        // into the body (relative to the car's own motion at that point).
        let hulls: Vec<(Vec3, Vec3, Vec3, f32, [(Vec3, Quat, Vec3); 2])> = cars
            .iter()
            .filter_map(|(_, c, a)| c.map(|c| &c.0).or(a.map(|a| &a.0)))
            .map(|v| {
                let (pos, _) = v.render_pose(alpha);
                let reach = (v.data.bbox[1] - v.data.bbox[0]).length() * 0.5 + 0.3;
                (pos, v.velocity, v.angular_velocity, reach, hull_boxes(v, alpha, 0.05))
            })
            .collect();
        for p in smoke.puffs.iter_mut().filter(|p| p.age < 3.0) {
            for (cpos, cvel, cang, reach, boxes) in &hulls {
                if p.pos.distance_squared(*cpos) > reach * reach {
                    continue;
                }
                for (c, r, h) in boxes {
                    let l = r.inverse() * (p.pos - *c);
                    if l.x.abs() >= h.x || l.y.abs() >= h.y || l.z.abs() >= h.z {
                        continue;
                    }
                    // Faces: +-X sides, +-Z ends, +Y roof (never down into the road).
                    let cands = [(h.x - l.x, Vec3::X), (h.x + l.x, Vec3::NEG_X), (h.z - l.z, Vec3::Z), (h.z + l.z, Vec3::NEG_Z), (h.y - l.y, Vec3::Y)];
                    let (d, n) = cands.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap_or((0.0, Vec3::X));
                    let nw = *r * n;
                    p.pos += nw * (d + 0.01);
                    let rel = p.vel - (*cvel + cang.cross(p.pos - *cpos));
                    let into = rel.dot(nw);
                    if into < 0.0 {
                        p.vel -= nw * into;
                    }
                }
            }
        }
    }

    let weights = smoke.weights.clone().unwrap_or_default();
    // Player only (user 2026-10-07: "tyre smoke for AI completely removed for now"); FH1_AI_SMOKE=1 = AI / traffic too.
    let ai_too = std::env::var("FH1_AI_SMOKE").is_ok_and(|v| v == "1");
    for (e, car, ai) in &cars {
        let Some(v) = car.map(|c| &c.0).or(if ai_too { ai.map(|a| &a.0) } else { None }) else { continue };
        emit_car(smoke, e, v, &weights, alpha, dt, t);
    }
}

fn emit_car(smoke: &mut Smoke, e: Entity, v: &Vehicle, weights: &[f32], alpha: f32, dt: f32, t_now: f32) {
    let (pos, rot) = v.render_pose(alpha);
    let up = rot * Vec3::Y;
    for (wi, w) in v.wheels.iter().enumerate() {
        let weight = if weights.is_empty() { 1.0 } else { weights.get(w.surface as usize).copied().unwrap_or(0.0) };
        if !w.grounded || weight <= 0.0 {
            smoke.carry.remove(&(e, wi));
            smoke.sustain.remove(&(e, wi));
            continue;
        }
        let r = v.data.tyre_radius[wi / 2];
        let hub = Vec3::from(v.data.hubs[wi]) - v.cg_model + Vec3::Y * v.wheel_drop(wi);
        let hub_w = pos + rot * hub;
        let contact = hub_w - up * r;
        let fwd = rot * (Quat::from_rotation_y(w.steer) * Vec3::NEG_Z);
        let right = fwd.cross(up).normalize_or_zero();
        let v_point = v.velocity + v.angular_velocity.cross(rot * hub);
        let tread = (v_point.dot(fwd) - w.omega * r) * fwd + v_point.dot(right) * right;
        // FH1_SMOKE_DEBUG=1: every grounded wheel smokes as in a full slide (screenshots / look checks).
        let slip = if debug() { SLIP_START + SLIP_FULL } else { tread.length() };
        // Sustained slip (module doc): the start threshold rises with speed, and smoke ramps in only after the slip has
        // held for a moment.
        let start = if sustain_on() { SLIP_START + knob_speed_slip() * v.velocity.length() } else { SLIP_START };
        let strict = strict_on() && !debug();
        // Sliding, not squealing: slip relative to the car's speed (module doc / strict_on).
        let ratio = slip / v.velocity.length().max(5.0);
        let slide = if strict {
            let x = ((ratio - knob_slip_ratio()) / 0.2).clamp(0.0, 1.0);
            x * x * (3.0 - 2.0 * x)
        } else {
            1.0
        };
        let mut gate = 1.0;
        if sustain_on() && !debug() {
            let t = smoke.sustain.entry((e, wi)).or_insert(0.0);
            if slip > start && (!strict || ratio > knob_slip_ratio()) {
                *t = (*t + dt).min(1.0);
            } else {
                *t = (*t - 3.0 * dt).max(0.0);
            }
            let (delay, ramp) = if strict { (0.3, 0.4) } else { (0.15, 0.3) };
            let x = ((*t - delay) / ramp).clamp(0.0, 1.0);
            gate = x * x * (3.0 - 2.0 * x);
        }
        let k = ((slip - start) / SLIP_FULL).clamp(0.0, 1.0) * weight.min(1.0) * gate * slide;
        if k <= 0.0 {
            smoke.carry.remove(&(e, wi));
            continue;
        }
        // Puffs per metre of tread slid (a burnout slides the tread without the car moving).
        let spacing = SPACING / (k * smoke.amount).max(0.05);
        let carry = smoke.carry.entry((e, wi)).or_insert(0.0);
        // Emission follows the tread slip up to 5 m/s (~16 puffs/s per wheel at full smoke): faster slides space the
        // puffs out along the path instead of piling hundreds into a whiteout.
        // Standing burnout: at most FH1_SMOKE_STILL_RATE puffs/s per wheel at 0 m/s, the full rate (~17/s) by 10 m/s.
        let speed = v.velocity.length();
        let rate_k = if burnout_fix() {
            let full = 5.0 / spacing.max(1e-3);
            let cap = knob_still_rate() + (full - knob_still_rate()) * (speed / 10.0).min(1.0);
            (cap / full).min(1.0)
        } else {
            1.0
        };
        let breakup = breakup_on() && !debug();
        // Per-wheel gust (break-up): two slow sines with a per-car / per-wheel phase, 0..1.
        let gust = if breakup {
            let ph = (e.to_bits() % 997) as f32 * 0.37 + wi as f32 * 1.7;
            (0.5 + 0.3 * (t_now * 2.3 + ph).sin() + 0.2 * (t_now * 5.1 + ph * 1.9).sin()).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let gust_rate = if breakup { 0.45 + 1.1 * gust } else { 1.0 };
        *carry += slip.min(5.0) * dt * rate_k * gust_rate;
        let mut c = *carry;
        let side = if wi % 2 == 0 { -1.0 } else { 1.0 };
        let mut emitted = 0usize;
        while c >= spacing && emitted < 6 {
            let i = emitted;
            emitted += 1;
            // Uneven gaps (break-up): 0.45-1.55 x the spacing, mean 1.
            c -= spacing * if breakup { 0.45 + 1.1 * smoke.rand() } else { 1.0 };
            let n = emitted + (c / spacing).max(0.0) as usize;
            if smoke.puffs.len() >= smoke.max {
                // Full: recycle the oldest (most faded) puff.
                if let Some((idx, _)) = smoke.puffs.iter().enumerate().max_by(|a, b| (a.1.age / a.1.life).total_cmp(&(b.1.age / b.1.life))) {
                    smoke.puffs.swap_remove(idx);
                }
            }
            let f = (i as f32 + smoke.rand()) / n.max(1) as f32;
            // Spread along this frame's contact path; thrown back off the tread, mostly kept with the car.
            let mut p0 = contact - v_point * dt * f + right * (side * 0.08) + up * 0.15;
            if breakup {
                // Anywhere over the tread / contact patch, not one point.
                p0 += right * (smoke.rand_s() * 0.12) + fwd * (smoke.rand_s() * 0.18) + up * (smoke.rand() * 0.1);
            }
            let jitter = Vec3::new(smoke.rand_s(), smoke.rand() * 0.6, smoke.rand_s()) * if breakup { 0.9 } else { 0.6 };
            let still = burnout_fix() && speed < 5.0;
            let lift = if still { 1.8 } else { 1.0 };
            let vel = v_point * 0.35 - tread * 0.12 + up * 0.6 * lift + right * (side * 0.4 * lift) + jitter * lift;
            // Wisps to clumps (break-up): mostly 0.55-1.2x, now and then a 1.6x+ clump; mean ~1.
            let (shape, dens) = if breakup {
                let r = smoke.rand();
                let clump = if smoke.rand() < 0.12 { 0.6 } else { 0.0 };
                (0.55 + 0.9 * r * r + clump, (0.65 + 0.6 * smoke.rand()) * (0.75 + 0.5 * gust))
            } else {
                (1.0, 1.0)
            };
            let (size, life) = (smoke.size * shape, smoke.life);
            let puff = Puff {
                pos: p0,
                vel,
                age: 0.0,
                life: (4.0 + 5.0 * smoke.rand()) * (0.6 + 0.4 * k) * life * if still { 0.65 } else { 1.0 },
                size0: if sustain_on() { (0.25 + 0.15 * smoke.rand()) * size } else { (0.5 + 0.3 * smoke.rand()) * size },
                size1: (3.0 + 3.0 * smoke.rand()) * (0.5 + 0.5 * k) * size,
                rot: smoke.rand() * std::f32::consts::TAU,
                spin: smoke.rand_s() * 0.35,
                seed: smoke.rand(),
                density: (0.25 + 0.45 * k) * smoke.opacity * dens,
                ground: contact.y,
                lift,
            };
            smoke.puffs.push(puff);
        }
        if emitted == 6 {
            c = c.min(spacing);
        }
        if let Some(cr) = smoke.carry.get_mut(&(e, wi)) {
            *cr = c.max(0.0);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub struct SmokeParams {
    /// xyz = unit vector toward the sun, w = sun illuminance (lux).
    sun: Vec4,
    /// rgb = sun colour (linear), w = sky ambient illuminance (lux).
    sun_colour: Vec4,
    /// rgb = smoke albedo, w = ground fade height (m).
    albedo: Vec4,
    /// x = volume mode, y = boxes in use, z = near clip (m), w = detail noise strength (smoke.wgsl).
    mode: Vec4,
    /// World -> unit cube of each car box (smoke.wgsl `ray_box`).
    boxes: [Mat4; MAX_BOXES],
}

const MAX_BOXES: usize = 8;

fn volume() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_VOLUME").map_or(true, |v| v != "0"))
}

fn push_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SMOKE_PUSH").map_or(true, |v| v != "0"))
}

/// A car's hull as two boxes in world space (centre, rotation, half extents), stacked without overlap (smoke.wgsl
/// subtracts each box's share of a view ray, so overlap would remove smoke twice): the lower body (93 % width, 95 %
/// length, bottom 50 % of the height) and the cabin on top of it (75 % width, 50-88 % of the height, the middle of the
/// length). Kept inside the body (shrunk, user saw the box edges 2026-10-08: the PristineBoundingBox top sits above the
/// roof). OUR approximation from gamedb's PristineBoundingBox (model space, front -Z), `margin` added on every side.
fn hull_boxes(v: &Vehicle, alpha: f32, margin: f32) -> [(Vec3, Quat, Vec3); 2] {
    let (pos, rot) = v.render_pose(alpha);
    let [a, b] = v.data.bbox;
    let size = (b - a).max(Vec3::splat(0.1));
    let lower_min = Vec3::new(a.x * 0.93, a.y, a.z + 0.025 * size.z);
    let lower_max = Vec3::new(b.x * 0.93, a.y + 0.5 * size.y, b.z - 0.025 * size.z);
    let cabin_min = Vec3::new(a.x * 0.75, a.y + 0.5 * size.y, a.z + 0.3 * size.z);
    let cabin_max = Vec3::new(b.x * 0.75, a.y + 0.88 * size.y, b.z - 0.2 * size.z);
    [(lower_min, lower_max), (cabin_min, cabin_max)].map(|(lo, hi)| {
        let c = (lo + hi) * 0.5 - v.cg_model;
        (pos + rot * c, rot, (hi - lo) * 0.5 + Vec3::splat(margin))
    })
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct SmokeMaterial {
    #[texture(0)]
    #[sampler(1)]
    noise: Handle<Image>,
    #[uniform(2)]
    params: SmokeParams,
}

impl Material for SmokeMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/smoke.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/smoke.wgsl".into()
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
            Mesh::ATTRIBUTE_UV_1.at_shader_location(2),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(3),
            Mesh::ATTRIBUTE_TANGENT.at_shader_location(4),
        ])?];
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                // Premultiplied alpha.
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::OneMinusSrcAlpha, operation: BlendOperation::Add },
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

/// Tiling fbm value noise, 256², two channels (R = billow, G = detail at another offset).
fn noise_image() -> Image {
    // 512² / 6 octaves since the volume pass (sharper close up; ~10 ms once at startup).
    let (n, octaves) = if volume() { (512usize, 6u32) } else { (256, 5) };
    let hash = |x: i32, y: i32, s: u32| -> f32 {
        let mut h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263) ^ s.wrapping_mul(0x9E37_79B9);
        h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
        ((h ^ (h >> 16)) & 0xFFFF) as f32 / 65535.0
    };
    let value = |x: f32, y: f32, period: i32, s: u32| {
        let (xi, yi) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - xi as f32, y - yi as f32);
        let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let w = |a: i32| a.rem_euclid(period);
        let a = hash(w(xi), w(yi), s);
        let b = hash(w(xi + 1), w(yi), s);
        let c = hash(w(xi), w(yi + 1), s);
        let d = hash(w(xi + 1), w(yi + 1), s);
        a + (b - a) * ux + (c - a) * uy + (a - b - c + d) * ux * uy
    };
    let fbm = |px: usize, py: usize, s: u32| {
        let (mut sum, mut amp, mut norm) = (0.0, 0.5, 0.0);
        for o in 0..octaves {
            let period = 4 << o;
            let f = period as f32 / n as f32;
            sum += value(px as f32 * f, py as f32 * f, period, s + o) * amp;
            norm += amp;
            amp *= 0.5;
        }
        sum / norm
    };
    let mut data = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            // Billowy: 1 - |2n - 1| ridges inverted into puffs.
            let a = fbm(x, y, 1);
            let b = fbm(x, y, 77);
            let billow = 1.0 - (2.0 * a - 1.0).abs();
            data.extend_from_slice(&[(billow * 255.0) as u8, (b * 255.0) as u8, (a * 255.0) as u8, 255]);
        }
    }
    let mut img = Image::new(
        Extent3d { width: n as u32, height: n as u32, depth_or_array_layers: 1 },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..default()
    });
    img
}

#[allow(clippy::too_many_arguments)]
fn draw(
    mut commands: Commands,
    mut smoke: ResMut<Smoke>,
    cams: Query<(&GlobalTransform, &Camera), With<fh1_render::post::FxPostCamera>>,
    suns: Query<(&DirectionalLight, &GlobalTransform)>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<SmokeMaterial>>,
    mut vis: Query<(&mut Visibility, &mut Transform, &mut GlobalTransform), (Without<fh1_render::post::FxPostCamera>, Without<DirectionalLight>)>,
    cars: Query<(Option<&Car>, Option<&AiCar>)>,
    fixed: Res<Time<Fixed>>,
    time: Res<Time>,
) {
    let smoke = &mut *smoke;
    let cam = cams.iter().find(|c| c.1.is_active).map_or(Vec3::ZERO, |c| c.0.translation());
    // Camera velocity (stretch is motion relative to the camera); a teleport / cut gives none.
    let dt = time.delta_secs().max(1e-4);
    let cam_vel = smoke.last_cam.map_or(Vec3::ZERO, |l| (cam - l) / dt);
    let cam_vel = if cam_vel.length() > 120.0 { Vec3::ZERO } else { cam_vel };
    smoke.last_cam = Some(cam);
    // The strongest directional light = the sun.
    let (sun_dir, sun_col, lux) = suns
        .iter()
        .max_by(|a, b| a.0.illuminance.total_cmp(&b.0.illuminance))
        .map_or((Vec3::Y, Vec3::ONE, 10_000.0), |(l, t)| (t.back().as_vec3(), l.color.to_linear().to_vec3(), l.illuminance));
    // Sky light: brighter with a higher sun; a floor so night smoke isn't pitch black.
    let ambient = lux * (0.12 + 0.18 * sun_dir.y.max(0.0)) + 30.0;
    let mut params = SmokeParams {
        sun: sun_dir.extend(lux),
        sun_colour: sun_col.extend(ambient),
        albedo: Vec3::splat(0.82).extend(0.5),
        mode: Vec4::new(if volume() { 1.0 } else { 0.0 }, 0.0, 0.3, 0.35),
        boxes: [Mat4::ZERO; MAX_BOXES],
    };
    if volume() && !smoke.puffs.is_empty() {
        // The cars nearest the camera (within 80 m): 2 boxes each.
        let alpha = fixed.overstep_fraction();
        let mut near: Vec<(f32, &Vehicle)> = cars
            .iter()
            .filter_map(|(c, a)| c.map(|c| &c.0).or(a.map(|a| &a.0)))
            .map(|v| (v.position.distance_squared(cam), v))
            .filter(|(d, _)| *d < 80.0 * 80.0)
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut k = 0;
        for (_, v) in near.into_iter().take(MAX_BOXES / 2) {
            for (c, r, h) in hull_boxes(v, alpha, 0.0) {
                params.boxes[k] = Mat4::from_scale_rotation_translation(h, r, c).inverse();
                k += 1;
            }
        }
        params.mode.y = k as f32;
    }

    let n = smoke.puffs.len();
    if n == 0 {
        if let Some((ent, ..)) = smoke.draw {
            if let Ok((mut v, ..)) = vis.get_mut(ent) {
                v.set_if_neq(Visibility::Hidden);
            }
        }
        return;
    }
    smoke.order.clear();
    let mut centroid = Vec3::ZERO;
    for (i, p) in smoke.puffs.iter().enumerate() {
        smoke.order.push((-(p.pos - cam).length_squared(), i as u32));
        centroid += p.pos;
    }
    centroid /= n as f32;
    smoke.order.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let order = std::mem::take(&mut smoke.order);
    smoke.pos.clear();
    smoke.corner.clear();
    smoke.size_rot.clear();
    smoke.colour.clear();
    smoke.extra.clear();
    for &(_, i) in &order {
        let p = smoke.puffs[i as usize];
        let u = p.age / p.life;
        // Fast puff-out, then slow expansion.
        let grow = 1.0 - (1.0 - u).powf(2.6);
        let size = p.size0 + (p.size1 - p.size0) * grow;
        // Fade in quickly, thin as it expands, fade out over the last 55 %.
        let fade_in = (p.age / 0.12).min(1.0);
        let fade_out = 1.0 - ((u - 0.45) / 0.55).clamp(0.0, 1.0).powf(1.3);
        let thin = (p.size0 / size).powf(0.45);
        let mut a = p.density * fade_in * fade_out * thin;
        if burnout_fix() {
            // Camera fade (module doc): none drawn inside half the radius.
            let r = size * 0.5;
            let d = p.pos.distance(cam);
            if d < 0.5 * r {
                continue;
            }
            let x = ((d - 0.5 * r) / (0.8 * r + 1.5)).clamp(0.0, 1.0);
            a *= x * x * (3.0 - 2.0 * x);
            if a < 0.003 {
                continue;
            }
        }
        let local = (p.pos - centroid).to_array();
        for corner in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
            smoke.pos.push(local);
            smoke.corner.push(corner);
            smoke.size_rot.push([size * 0.5, p.rot]);
            smoke.colour.push([p.seed, a, u, p.ground]);
            // xyz = motion relative to the camera x the stretch time (m of smear).
            // Per-puff stretch (break-up): 0.5-1.5x, so a fast trail isn't a row of identical ellipsoids.
            let vary = if breakup_on() { 0.5 + (p.seed * 7.31).fract() } else { 1.0 };
            let smear = (p.vel - cam_vel) * stretch_secs() * vary;
            smoke.extra.push([smear.x, smear.y, smear.z, 0.0]);
        }
    }
    smoke.order = order;
    // Quads actually drawn (camera-faded puffs are skipped).
    let n = smoke.pos.len() / 4;
    match &smoke.draw {
        Some((ent, mesh, mat)) => {
            if let Some(mut m) = meshes.get_mut(mesh) {
                fill(&mut m, smoke, n);
            }
            if materials.get(mat).is_some_and(|m| m.params != params) {
                if let Some(mut m) = materials.get_mut(mat) {
                    m.params = params;
                }
            }
            if let Ok((mut v, mut t, mut gt)) = vis.get_mut(*ent) {
                v.set_if_neq(Visibility::Inherited);
                t.translation = centroid;
                // After propagation: write the GlobalTransform the renderer extracts this frame.
                *gt = GlobalTransform::from_translation(centroid);
            }
        }
        None => {
            let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
            m.insert_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_UV_0, Vec::<[f32; 2]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_UV_1, Vec::<[f32; 2]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_TANGENT, Vec::<[f32; 4]>::new());
            m.insert_indices(Indices::U32(Vec::new()));
            fill(&mut m, smoke, n);
            let mesh = meshes.add(m);
            let mat = materials.add(SmokeMaterial { noise: images.add(noise_image()), params });
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
                    Name::new("Remaster tyre smoke"),
                ))
                .id();
            smoke.draw = Some((ent, mesh, mat));
        }
    }
}

fn fill(m: &mut Mesh, s: &Smoke, n: usize) {
    fn put<T: Copy>(dst: &mut Vec<T>, src: &[T]) {
        dst.clear();
        dst.extend_from_slice(src);
    }
    if let Some(VertexAttributeValues::Float32x3(v)) = m.attribute_mut(Mesh::ATTRIBUTE_POSITION) {
        put(v, &s.pos);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        put(v, &s.corner);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_1) {
        put(v, &s.size_rot);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_COLOR) {
        put(v, &s.colour);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_TANGENT) {
        put(v, &s.extra);
    }
    if let Some(Indices::U32(idx)) = m.indices_mut() {
        let want = n * 6;
        if idx.len() > want {
            idx.truncate(want);
        }
        for q in idx.len() / 6..n {
            let b = q as u32 * 4;
            idx.extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
        }
    }
}
