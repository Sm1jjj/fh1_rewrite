//! FH1's car headlights on the world: the deferred headlight pass (RE: docs/SHADERS.md "Car headlights").
//!
//! The game draws up to 8 car lights into the screen shadow mask's .yzw as `exp2(−4·Σcol)` and the
//! track/car pixel shaders decode `−log2(mask.yzw)` behind `bEnableDeferredLightContribution` (b228) /
//! `psDeferredHeadlightEnable` (b142). Like the sun mask (`shadow.rs` `fx_shadow_mask`), the light
//! pass is evaluated per fragment here: [`FX_HEADLIGHT_WGSL`] `fx_headlight_yzw()` returns the
//! value the game's mask would hold. Light records and the dip-beam texture live in one storage
//! buffer ([`HEADLIGHT_BUFFER`], FxMaterial and FxCarMaterial binding 40).
//!
//! Cars opt in with [`FxHeadlightSource`]; [`FxHeadlightState`] (required by it) holds the per-car
//! on-state (`amount` = SetHeadLightAmount, for the lens emissive). Default = the game's switch-on
//! (night cost ~1.5 ms while driving, measured interleaved with `FH1_HEADLIGHT_DEBUG=cycle`);
//! `FH1_HEADLIGHTS=1` forces them on at any time of day, `=0` = off.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::storage::ShaderBuffer;

use crate::lighting::FxTimeOfDay;
use crate::{FxCarGlobals, FxGlobals};

pub struct FxHeadlightPlugin;

impl Plugin for FxHeadlightPlugin {
    fn build(&self, app: &mut App) {
        let bytes = HeadlightBuffer::default().bytes(0.0);
        let _ = app.world_mut().resource_mut::<Assets<ShaderBuffer>>().insert(HEADLIGHT_BUFFER.id(), ShaderBuffer::new(&bytes, RenderAssetUsages::default()));
        app.init_resource::<HeadlightBuffer>()
            .init_resource::<FxHeadlightFrame>()
            .init_resource::<HeadlightDebug>()
            .add_systems(Startup, load_beam)
            .add_systems(Update, cycle_debug)
            .add_systems(
                PostUpdate,
                (update_headlight_state, update_headlights)
                    .chain()
                    .after(crate::lighting::update_car_lighting)
                    .before(crate::upload_globals)
                    .after(TransformSystems::Propagate),
            );
    }
}

/// The storage buffer every FxMaterial and FxCarMaterial binds at 40 (`fx_headlights` in [`FX_HEADLIGHT_WGSL`]).
pub const HEADLIGHT_BUFFER: Handle<ShaderBuffer> = bevy::asset::uuid_handle!("3b8e6f0a-41d2-4c7e-9f35-8a2d6c1e7b94");

/// At most 8 lights (0x82DDE170; AI cars count).
pub const MAX_LIGHTS: usize = 8;
const BEAM_W: usize = 256;
const BEAM_H: usize = 128;

/// A car that casts headlights. `lamp` = the light origin in the entity's local space (INFERRED:
/// the game stores (car+0x770 + car+0x780)/2, the midpoint of the two lamps).
#[derive(Component, Clone, Copy, Debug)]
#[require(FxHeadlightState)]
pub struct FxHeadlightSource {
    pub player: bool,
    pub lamp: Vec3,
}

impl Default for FxHeadlightSource {
    fn default() -> Self {
        Self { player: true, lamp: Vec3::new(0.0, 0.0, -2.0) }
    }
}

/// Per-car light state (per-car update 0x8249E340, VERIFIED): lights are wanted when the clock is
/// before 08:10 or after 18:50; the state follows with probability 0.05 per update (random stagger).
/// `amount` = lightsOn && !popupAnimating (pop-ups not modelled), what SetHeadLightAmount gets.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FxHeadlightState {
    pub on: bool,
    pub amount: f32,
    rng: u32,
}

/// This frame's headlight globals, for the car shaders (fb: psDeferredHeadlightEnable b142,
/// dynamicLights4 c110 / dynamicLights5 c111 are INFERRED = params1 / params2.x).
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct FxHeadlightFrame {
    pub count: u32,
    /// HeadLightParams1 c168: light 0's matrix × (0, 2, 1) (game space; engine (0, 2, −1)).
    pub params1: Vec4,
    /// HeadLightParams2 c169: (G.44 / G.40 = 0.5, G.40 = 4, player light range, 0).
    pub params2: Vec4,
}

/// One light record as the shader reads it: world → light rows, (1/frustumX, 1/frustumY, range,
/// near), (tint, 0).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct LightRecord {
    rows: [Vec4; 3],
    params: Vec4,
    colour: Vec4,
}

#[derive(Resource)]
struct HeadlightBuffer {
    lights: Vec<LightRecord>,
    /// 256×128 RGBA8 dip beam, packed little-endian (r in the low byte).
    beam: Vec<u32>,
    dirty: bool,
}

impl Default for HeadlightBuffer {
    fn default() -> Self {
        Self { lights: Vec::new(), beam: vec![0; BEAM_W * BEAM_H], dirty: true }
    }
}

impl HeadlightBuffer {
    fn bytes(&self, debug: f32) -> Vec<u8> {
        let mut f: Vec<f32> = Vec::with_capacity(4 + MAX_LIGHTS * 20);
        f.extend([self.lights.len().min(MAX_LIGHTS) as f32, ENCODE, debug, 0.0]);
        for k in 0..MAX_LIGHTS {
            let l = self.lights.get(k).copied().unwrap_or_default();
            for v in l.rows.iter().chain([&l.params, &l.colour]) {
                f.extend(v.to_array());
            }
        }
        let mut out: Vec<u8> = f.iter().flat_map(|x| x.to_le_bytes()).collect();
        out.extend(self.beam.iter().flat_map(|x| x.to_le_bytes()));
        out
    }
}

// Defaults from G = 0x832B8B20 (VERIFIED; a record's zero fields fall back to these).
const RANGE_PLAYER: f32 = 60.0;
const RANGE_OTHER: f32 = 25.0;
const FRUSTUM_X: f32 = 2.2;
const FRUSTUM_Y: f32 = 0.3;
const NEAR: f32 = 1.5;
/// G.10 = (0, 0, −1) game car space, added to the lamp midpoint before the car matrix (82de0680):
/// 1 m behind the lamps (engine cars face −Z, so +Z here).
const LAMP_OFFSET: Vec3 = Vec3::new(0.0, 0.0, 1.0);
/// G.40: the mask encode scale (out = exp2(−G.40·col)).
const ENCODE: f32 = 4.0;
/// G.44: the decode gain numerator (c169.x = G.44 / G.40).
const DECODE: f32 = 2.0;
/// Light type 1 colour (0x8245C4B8; type names INFERRED, every car uses type 1 until the car's type is read).
const TINT: Vec3 = Vec3::new(1.0, 1.0, 0.8);
/// Auto lights window (0x8249E340): on before 08:10 and after 18:50.
const AUTO_OFF_FROM: f32 = 29_400.0;
const AUTO_ON_FROM: f32 = 67_800.0;
const FLIP_CHANCE: f32 = 0.05;

fn load_beam(config: Option<Res<crate::postfx::FxPostConfig>>, mut buffer: ResMut<HeadlightBuffer>) {
    let Some(config) = config else { return };
    let Some(root) = config.0.xex_dir.parent().and_then(std::path::Path::parent) else { return };
    let path = root.join("cars/carlights/headlight_beam_dip.dds");
    match read_beam(&path) {
        Some(b) => {
            buffer.beam = b;
            buffer.dirty = true;
        }
        None => warn!("fh1-render headlights: {} missing or not 256×128 RGBA8 (run fh1setup cars)", path.display()),
    }
}

/// The installed beam: a DX10 DDS, R8G8B8A8_UNORM (28), 256×128, one mip.
fn read_beam(path: &std::path::Path) -> Option<Vec<u32>> {
    let b = std::fs::read(path).ok()?;
    let u = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    if b.get(..4)? != b"DDS " || b.get(84..88)? != b"DX10" || u(128)? != 28 || (u(12)?, u(16)?) != (BEAM_H as u32, BEAM_W as u32) {
        return None;
    }
    let px = b.get(148..148 + BEAM_W * BEAM_H * 4)?;
    Some(px.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
}

/// `FH1_HEADLIGHT_DEBUG` (perf A/B): 1 = beam texture fetch skipped (beam 1), 2 = light loop skipped,
/// 3 = no lights at all (b228 off); `cycle` = 0..3 in turn, 5 s each, logging each mode's mean frame time.
#[derive(Resource)]
struct HeadlightDebug {
    mode: u32,
    cycle: bool,
    t: f32,
    warm: bool,
    /// Per mode: (Σ ms, frames) since the start (the first second after a switch is skipped).
    acc: [(f32, u32); 4],
}

impl Default for HeadlightDebug {
    fn default() -> Self {
        let v = std::env::var("FH1_HEADLIGHT_DEBUG").unwrap_or_default();
        Self { mode: v.parse().unwrap_or(0).min(3), cycle: v == "cycle", t: 0.0, warm: false, acc: [(0.0, 0); 4] }
    }
}

fn cycle_debug(time: Res<Time<Real>>, mut d: ResMut<HeadlightDebug>, mut buffer: ResMut<HeadlightBuffer>) {
    if !d.cycle {
        return;
    }
    let dt = time.delta_secs();
    d.t += dt;
    if d.t > 1.0 {
        let m = d.mode as usize;
        d.acc[m].0 += dt * 1000.0;
        d.acc[m].1 += 1;
    }
    if d.t >= 5.0 {
        d.t = 0.0;
        d.mode = (d.mode + 1) % 4;
        buffer.dirty = true;
        if d.mode == 0 && !d.warm {
            // The first cycle includes loading: drop it.
            d.warm = true;
            d.acc = [(0.0, 0); 4];
        } else if d.mode == 0 {
            let s: Vec<String> = d.acc.iter().enumerate().map(|(k, (ms, n))| format!("{k}: {:.2} ms ({n})", ms / (*n).max(1) as f32)).collect();
            info!("headlight A/B mean frame time: {}", s.join(", "));
        }
    }
}

fn mode() -> Option<bool> {
    match std::env::var("FH1_HEADLIGHTS").as_deref() {
        Ok("0") => Some(false),
        Ok("1") => Some(true),
        _ => None,
    }
}

fn update_headlight_state(tod: Option<Res<FxTimeOfDay>>, mut cars: Query<(Entity, &mut FxHeadlightState)>) {
    let forced = mode();
    let seconds = tod.map_or(57_600.0, |t| t.seconds);
    let want = forced.unwrap_or(!(AUTO_OFF_FROM..=AUTO_ON_FROM).contains(&seconds));
    for (e, mut s) in &mut cars {
        if s.rng == 0 {
            s.rng = e.index_u32().wrapping_mul(0x9E37_79B9) | 1;
        }
        if s.on != want {
            // xorshift32: the game rolls rand < 0.05 per update.
            let mut x = s.rng;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            s.rng = x;
            if forced.is_some() || (x >> 8) as f32 / (1u32 << 24) as f32 <= FLIP_CHANCE {
                s.on = want;
            }
        }
        let amount = if s.on { 1.0 } else { 0.0 };
        if s.amount != amount {
            s.amount = amount;
        }
    }
}

/// World → light rows for a light at `origin` looking along the car's forward axis. Light space is
/// the game's (left-handed): x right, y up, z forward. Engine cars face −Z with +X right.
fn light_rows(car: &GlobalTransform, origin: Vec3) -> [Vec4; 3] {
    let (_, rot, _) = car.to_scale_rotation_translation();
    let right = rot * Vec3::X;
    let up = rot * Vec3::Y;
    let fwd = rot * Vec3::NEG_Z;
    [right, up, fwd].map(|a| a.extend(-a.dot(origin)))
}

fn update_headlights(
    sources: Query<(&GlobalTransform, &FxHeadlightSource, &FxHeadlightState)>,
    cams: Query<&GlobalTransform, With<crate::post::FxPostCamera>>,
    mut buffer: ResMut<HeadlightBuffer>,
    mut frame: ResMut<FxHeadlightFrame>,
    mut globals: ResMut<FxGlobals>,
    car_globals: Option<ResMut<FxCarGlobals>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    debug: Res<HeadlightDebug>,
) {
    let eye = cams.iter().next().map_or(Vec3::ZERO, |t| t.translation());
    // Gather (0x82471BA8): cars with lights on; sort 0x82DCC7F0 untraced (INFERRED player first, then nearest).
    let mut on: Vec<(&GlobalTransform, &FxHeadlightSource)> = sources.iter().filter(|(_, _, s)| s.amount > 0.0).map(|(t, h, _)| (t, h)).collect();
    on.sort_by(|a, b| b.1.player.cmp(&a.1.player).then(a.0.translation().distance_squared(eye).total_cmp(&b.0.translation().distance_squared(eye))));
    on.truncate(if debug.mode == 3 { 0 } else { MAX_LIGHTS });
    let lights: Vec<LightRecord> = on
        .iter()
        .enumerate()
        .map(|(k, (t, h))| {
            let range = if k == 0 { RANGE_PLAYER } else { RANGE_OTHER };
            LightRecord {
                rows: light_rows(t, t.transform_point(h.lamp + LAMP_OFFSET)),
                params: Vec4::new(1.0 / FRUSTUM_X, 1.0 / FRUSTUM_Y, range, NEAR),
                colour: TINT.extend(0.0),
            }
        })
        .collect();
    let new_frame = FxHeadlightFrame {
        count: lights.len() as u32,
        params1: on.first().map_or(Vec4::ZERO, |(t, _)| t.transform_point(Vec3::new(0.0, 2.0, -1.0)).extend(1.0)),
        params2: Vec4::new(DECODE / ENCODE, ENCODE, RANGE_PLAYER, 0.0),
    };
    if lights != buffer.lights {
        buffer.lights = lights;
        buffer.dirty = true;
    }
    if buffer.dirty {
        buffer.dirty = false;
        let bytes = buffer.bytes(debug.mode.min(2) as f32);
        if let Some(mut b) = buffers.get_mut(&HEADLIGHT_BUFFER) {
            b.data = Some(bytes);
        }
    }
    // Per frame (0x82471F38 → 0x8243A2F4): b228 = light count > 0 (routing INFERRED).
    let enable = new_frame.count > 0;
    if enable != (frame.count > 0) || globals.get("HeadLightParams1") != Some(new_frame.params1) {
        globals.set_bool("bEnableDeferredLightContribution", enable);
        globals.set_vec("HeadLightParams1", new_frame.params1);
        globals.set_vec("HeadLightParams2", new_frame.params2);
        if let Some(mut car) = car_globals {
            car.set_bool("psDeferredHeadlightEnable", enable);
        }
    }
    *frame = new_frame;
}

/// The per-fragment headlight pass (light pass PS 0x82184E24, VERIFIED): for each light
/// `L = lightTransform·P; uv = (0.5 + 0.5·c5.x·L.x/L.z, 0.5 − 0.5·c5.y·L.y/L.z);
/// col = beam(uv).rgb·tint·falloff(|L|/range)·sat(L.z − near)`, mask.yzw = exp2(−4·Σcol) (blend
/// INFERRED multiplicative onto a cleared 1). falloff = tf14's procedural ramp 1 − t²(3 − 2t).
/// Beam addressing outside 0..1 INFERRED black (the texture's border is black). Needs
/// `fx_frag_pos` and Bevy's `view` (declared by `shadow::FX_SHADOW_WGSL`).
pub const FX_HEADLIGHT_WGSL: &str = r#"
struct FxHeadlight {
    r0: vec4<f32>,
    r1: vec4<f32>,
    r2: vec4<f32>,
    p: vec4<f32>,
    c: vec4<f32>,
}
struct FxHeadlights {
    head: vec4<f32>,
    recs: array<FxHeadlight, 8>,
    beam: array<u32, 32768>,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(40) var<storage, read> fx_headlights: FxHeadlights;

fn fx_beam_texel(t: vec2<i32>) -> vec3<f32> {
    let c = clamp(t, vec2<i32>(0), vec2<i32>(255, 127));
    return unpack4x8unorm(fx_headlights.beam[u32(c.y) * 256u + u32(c.x)]).rgb;
}

fn fx_beam(uv: vec2<f32>) -> vec3<f32> {
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) {
        return vec3<f32>(0.0);
    }
    let t = uv * vec2<f32>(256.0, 128.0) - 0.5;
    let b = vec2<i32>(floor(t));
    let f = fract(t);
    let top = mix(fx_beam_texel(b), fx_beam_texel(b + vec2<i32>(1, 0)), f.x);
    let bottom = mix(fx_beam_texel(b + vec2<i32>(0, 1)), fx_beam_texel(b + vec2<i32>(1, 1)), f.x);
    return mix(top, bottom, f.y);
}

// fx_shadow_mask() runs once per tf13 fetch: evaluate the lights once per fragment.
var<private> fx_headlight_memo: vec4<f32>;

fn fx_headlight_yzw() -> vec3<f32> {
    if fx_headlight_memo.w == 0.0 {
        fx_headlight_memo = vec4<f32>(fx_headlight_eval(), 1.0);
    }
    return fx_headlight_memo.xyz;
}

fn fx_headlight_eval() -> vec3<f32> {
    let n = min(u32(fx_headlights.head.x), 8u);
    if n == 0u || fx_frag_pos.z <= 0.0 || fx_headlights.head.z == 2.0 {
        return vec3<f32>(1.0);
    }
    let ndc = vec2<f32>((fx_frag_pos.x - view.viewport.x) / view.viewport.z * 2.0 - 1.0, 1.0 - (fx_frag_pos.y - view.viewport.y) / view.viewport.w * 2.0);
    let w = view.world_from_clip * vec4<f32>(ndc, fx_frag_pos.z, 1.0);
    let p = vec4<f32>(w.xyz / w.w, 1.0);
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < n; i++) {
        let l = fx_headlights.recs[i];
        let v = vec3<f32>(dot(l.r0, p), dot(l.r1, p), dot(l.r2, p));
        let d = length(v) / l.p.z;
        if v.z <= l.p.w || d >= 1.0 {
            continue;
        }
        let uv = vec2<f32>(0.5 + 0.5 * l.p.x * v.x / v.z, 0.5 - 0.5 * l.p.y * v.y / v.z);
        let falloff = 1.0 - d * d * (3.0 - 2.0 * d);
        var beam = vec3<f32>(1.0);
        if fx_headlights.head.z != 1.0 {
            beam = fx_beam(uv);
        }
        sum += beam * l.c.rgb * falloff * saturate(v.z - l.p.w);
    }
    return exp2(-fx_headlights.head.y * sum);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_layout() {
        let mut b = HeadlightBuffer::default();
        b.lights.push(LightRecord { rows: [Vec4::X, Vec4::Y, Vec4::Z], params: Vec4::new(1.0, 2.0, 3.0, 4.0), colour: Vec4::ONE });
        let bytes = b.bytes(0.0);
        // head + 8 × 5 vec4 + 256×128 u32, matching `FxHeadlights`.
        assert_eq!(bytes.len(), 16 + MAX_LIGHTS * 80 + BEAM_W * BEAM_H * 4);
        let f = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!((f(0), f(4)), (1.0, ENCODE));
        assert_eq!(f(16 + 48 + 8), 3.0);
    }

    #[test]
    fn light_space_axes() {
        // A car at the origin facing −Z: a point 10 m ahead and 1 m right is at L = (1, 0, 10).
        let t = GlobalTransform::from(Transform::from_xyz(0.0, 0.0, 0.0));
        let rows = light_rows(&t, Vec3::ZERO);
        let p = Vec4::new(1.0, 0.0, -10.0, 1.0);
        assert_eq!(rows.map(|r| r.dot(p)), [1.0, 0.0, 10.0]);
        // Turned to face +X.
        let t = GlobalTransform::from(Transform::from_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2)).with_translation(Vec3::new(5.0, 0.0, 0.0)));
        let rows = light_rows(&t, Vec3::new(5.0, 0.0, 0.0));
        let l = rows.map(|r| r.dot(Vec4::new(15.0, 1.0, 0.0, 1.0)));
        assert!((l[2] - 10.0).abs() < 1e-4 && (l[1] - 1.0).abs() < 1e-4 && l[0].abs() < 1e-4);
    }
}
