//! FH1 shading and lighting for the Bevy engine: materials that run the game's own shaders
//! (translated from the `.fxobj` microcode by `fh1-shaders`) and the global lighting state they
//! read. See `docs/SHADERS.md`.
//!
//! Use: add [`Fh1RenderPlugin`], load effects into [`FxLibrary`] (`program()`), create
//! materials with [`FxLibrary::material`], and set lighting through [`FxGlobals`] by the
//! game's parameter names (`sunDir`, `FogConsts`, ...).

pub mod car;
pub mod car_material;
pub mod car_shadow;
pub mod files;
pub mod lighting;
pub mod material;
pub mod post;
pub mod postfx;
pub mod program;
pub mod quality;
pub mod mirror;
pub mod reflect;
pub mod scenery;
pub mod shadow;
pub mod sky;
pub mod glow;
pub mod particles;
pub mod headlight;
pub mod tod;
pub mod wheel;

use std::collections::HashMap;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::pbr::MaterialPlugin;
use bevy::prelude::*;
use bevy::render::storage::ShaderBuffer;
use fh1_shaders::container::{RegisterSet, Stage};
use fh1_shaders::effect::Effect;

pub use material::{FxMaterial, FxMaterialConsts};
pub use program::Program;

pub struct Fh1RenderPlugin;

impl Plugin for Fh1RenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<FxMaterial>::default())
            .add_plugins(MaterialPlugin::<car_material::FxCarMaterial>::default())
            .add_systems(PostUpdate, (lighting::update_time_of_day, lighting::update_car_lighting, upload_globals, upload_car_globals).chain())
            .add_plugins(mirror::FxMirrorPlugin)
            .add_plugins(wheel::FxWheelPlugin)
            .add_plugins(glow::FxGlowPlugin)
            .add_plugins(particles::FxParticlesPlugin)
            .add_plugins(headlight::FxHeadlightPlugin)
            .add_systems(PostUpdate, postfx::update_post.after(lighting::update_time_of_day))
            .add_systems(PreStartup, postfx::init_post)
            // In-process map change: a new FxPostConfig rebuilds the chain and sends FxTrackChanged (glows, cube, fog).
            .add_message::<postfx::FxTrackChanged>()
            .add_systems(Update, (postfx::reload_post, lighting::reload_fog_templates).chain());
        // Remaster renderer (docs/REMASTER.md, fh1-remaster light/sky/post): Bevy lights/atmosphere/CSM/bloom replace the
        // game's post chain, live cube, shadow mask and sky. update_post keeps running (zones + the blended grading LUT).
        if remaster() {
            app.add_plugins(sky::FxSkyPlugin);
            // FxRawStandard stays registered (engine FxParams uses its assets); the swap is idle with raw_output off.
            car_material::add_raw_standard(app);
            app.add_plugins(car_shadow::drop_shadow::DropShadowPlugin);
        } else {
            app.add_plugins((post::FxPostPlugin, reflect::FxReflectPlugin, shadow::FxShadowPlugin, car_shadow::CarShadowPlugin, sky::FxSkyPlugin));
        }
        // Created here (not in a Startup system) so other Startup systems can use them.
        let globals = new_globals(&mut app.world_mut().resource_mut::<Assets<ShaderBuffer>>());
        let mut car_globals = new_globals(&mut app.world_mut().resource_mut::<Assets<ShaderBuffer>>());
        car_globals.all_defaults = true;
        app.insert_resource(globals).insert_resource(FxCarGlobals(car_globals)).init_resource::<FxLibrary>();
    }
}

/// Size of the global register file in bytes (vs + ps float4s, 256 bools as u32, 32 int4s).
const GLOBALS_BYTES: usize = 256 * 16 * 2 + 64 * 16 + 32 * 16;

/// The game's global shader parameters: one register file per stage, shared by every
/// material (the effects all use the same register for the same global).
#[derive(Resource)]
pub struct FxGlobals {
    pub vs: Vec<[f32; 4]>,
    pub ps: Vec<[f32; 4]>,
    /// Raw CF bool addresses: VS 0-127, PS 128-255.
    pub bools: Vec<u32>,
    pub ints: Vec<[i32; 4]>,
    /// Parameter name → (stage, register set, first register, count), learnt from programs.
    names: HashMap<String, Vec<(Stage, RegisterSet, u32, u32)>>,
    /// Last value set per name, re-applied when a later program declares the name.
    values: HashMap<String, Vec<[f32; 4]>>,
    bool_values: HashMap<String, bool>,
    buffer: Handle<ShaderBuffer>,
    dirty: bool,
    /// Initialise every declared register from its constant-table default (car bank), not only
    /// those at or above the track material range.
    all_defaults: bool,
}

/// The global bank for car shaders (their register allocation differs from the track effects').
#[derive(Resource, bevy::prelude::Deref, bevy::prelude::DerefMut)]
pub struct FxCarGlobals(pub FxGlobals);

impl FxGlobals {
    pub fn buffer(&self) -> Handle<ShaderBuffer> {
        self.buffer.clone()
    }

    /// Set a float parameter (all registers it occupies, in every stage that declares it).
    /// Returns false if no loaded program uses the name.
    pub fn set(&mut self, name: &str, values: &[[f32; 4]]) -> bool {
        self.values.insert(name.to_string(), values.to_vec());
        self.write(name, values)
    }

    fn write(&mut self, name: &str, values: &[[f32; 4]]) -> bool {
        let Some(locs) = self.names.get(name) else { return false };
        for &(stage, set, reg, count) in locs {
            if set != RegisterSet::Float4 {
                continue;
            }
            let file = if stage == Stage::Vertex { &mut self.vs } else { &mut self.ps };
            for (k, v) in values.iter().take(count as usize).enumerate() {
                if let Some(r) = file.get_mut(reg as usize + k) {
                    *r = *v;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// The value last set for a parameter (first register).
    pub fn get(&self, name: &str) -> Option<Vec4> {
        self.values.get(name).and_then(|v| v.first()).map(|v| Vec4::from_array(*v))
    }

    pub fn set_vec(&mut self, name: &str, v: Vec4) -> bool {
        self.set(name, &[v.to_array()])
    }

    /// Set a matrix parameter from a column-vector matrix (register k = row k).
    pub fn set_matrix(&mut self, name: &str, m: Mat4) -> bool {
        let rows = [m.row(0), m.row(1), m.row(2), m.row(3)].map(|r| r.to_array());
        self.set(name, &rows)
    }

    pub fn set_bool(&mut self, name: &str, value: bool) -> bool {
        self.bool_values.insert(name.to_string(), value);
        self.write_bool(name, value)
    }

    fn write_bool(&mut self, name: &str, value: bool) -> bool {
        let Some(locs) = self.names.get(name) else { return false };
        for &(stage, set, reg, _) in locs {
            if set == RegisterSet::Bool {
                let addr = if stage == Stage::Pixel { reg + 128 } else { reg };
                if let Some(b) = self.bools.get_mut(addr as usize) {
                    *b = value as u32;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// Parameter names known so far.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(|s| s.as_str())
    }

    fn learn(&mut self, effect: &Effect) {
        for b in &effect.shaders {
            for c in &b.constants {
                if c.set == RegisterSet::Sampler {
                    continue;
                }
                let entry = self.names.entry(c.name.clone()).or_default();
                let loc = (b.stage, c.set, c.register as u32, c.count.max(1) as u32);
                if entry.contains(&loc) {
                    continue;
                }
                entry.push(loc);
                // Initialise from the constant table's default.
                if let Some(d) = &c.default {
                    match c.set {
                        RegisterSet::Float4 if self.all_defaults || c.register as usize >= program::MATERIAL_REGS => {
                            let file = if b.stage == Stage::Vertex { &mut self.vs } else { &mut self.ps };
                            for k in 0..c.count as usize {
                                if d.len() >= k * 4 + 4 {
                                    if let Some(r) = file.get_mut(c.register as usize + k) {
                                        *r = [0, 1, 2, 3].map(|j| f32::from_bits(d[k * 4 + j]));
                                    }
                                }
                            }
                        }
                        RegisterSet::Bool => {
                            let addr = c.register as usize + if b.stage == Stage::Pixel { 128 } else { 0 };
                            if let (Some(slot), Some(v)) = (self.bools.get_mut(addr), d.first()) {
                                *slot = (*v != 0) as u32;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Re-apply values set before this program declared them.
        let pending: Vec<(String, Vec<[f32; 4]>)> = self.values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in pending {
            self.write(&k, &v);
        }
        let pending: Vec<(String, bool)> = self.bool_values.iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (k, v) in pending {
            self.write_bool(&k, v);
        }
        self.dirty = true;
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(GLOBALS_BYTES);
        for v in self.vs.iter().chain(&self.ps) {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        for b in &self.bools {
            out.extend_from_slice(&b.to_le_bytes());
        }
        for v in &self.ints {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        out
    }
}

fn new_globals(buffers: &mut Assets<ShaderBuffer>) -> FxGlobals {
    let mut g = FxGlobals {
        vs: vec![[0.0; 4]; 256],
        ps: vec![[0.0; 4]; 256],
        bools: vec![0; 256],
        ints: vec![[0; 4]; 32],
        names: HashMap::new(),
        values: HashMap::new(),
        bool_values: HashMap::new(),
        buffer: Handle::default(),
        dirty: true,
        all_defaults: false,
    };
    g.buffer = buffers.add(ShaderBuffer::new(&g.bytes(), RenderAssetUsages::default()));
    g
}

fn upload_car_globals(globals: Option<ResMut<FxCarGlobals>>, buffers: ResMut<Assets<ShaderBuffer>>) {
    let Some(mut g) = globals else { return };
    upload(&mut g.0, buffers);
}

fn upload_globals(globals: Option<ResMut<FxGlobals>>, buffers: ResMut<Assets<ShaderBuffer>>) {
    let Some(mut g) = globals else { return };
    upload(&mut g, buffers);
}

fn upload(g: &mut FxGlobals, mut buffers: ResMut<Assets<ShaderBuffer>>) {
    if !g.dirty {
        return;
    }
    g.dirty = false;
    let bytes = g.bytes();
    if let Some(mut b) = buffers.get_mut(&g.buffer) {
        b.data = Some(bytes);
    }
}

/// Loaded effects and their translated programs.
#[derive(Resource, Default)]
pub struct FxLibrary {
    /// Materials write the game's sqrt-encoded colour for the FH1 post chain (`post::FxPostCamera`)
    /// instead of linear colour. Set before the first program is built.
    pub raw_output: bool,
    effects: HashMap<String, Arc<Effect>>,
    programs: HashMap<(String, String), Option<(u32, Arc<Program>)>>,
}

impl FxLibrary {
    /// Parse an effect (once) under `name`.
    pub fn add_effect(&mut self, name: &str, bytes: &[u8]) -> Result<(), fh1_shaders::Error> {
        let key = name.to_ascii_lowercase();
        if !self.effects.contains_key(&key) {
            self.effects.insert(key, Arc::new(Effect::parse(bytes)?));
        }
        Ok(())
    }

    pub fn has_effect(&self, name: &str) -> bool {
        self.effects.contains_key(&name.to_ascii_lowercase())
    }

    /// The program for (effect, technique), translating and registering it on first use.
    pub fn program(&mut self, name: &str, technique: &str, shaders: &mut Assets<Shader>, globals: &mut FxGlobals) -> Option<(u32, Arc<Program>)> {
        self.program_family(name, technique, &program::Family::Track, shaders, globals)
    }

    /// A one-pass program from installed default.xex shader containers (`shaders/xex/<addr>.bin`) with the
    /// render states the host sets around the draw; `patch` adapts the WGSL (None = reject). Built and
    /// registered once per `label`. Used by fh1-engine anim.rs (PROC_ANIM_OBJ_*).
    #[allow(clippy::too_many_arguments)]
    pub fn xex_program(
        &mut self,
        dir: &std::path::Path,
        vs: u32,
        ps: Option<u32>,
        render_states: &[(u32, u32)],
        label: &str,
        shaders: &mut Assets<Shader>,
        globals: &mut FxGlobals,
        patch: impl FnOnce(&mut Program) -> Option<()>,
    ) -> Option<(u32, Arc<Program>)> {
        use fh1_shaders::effect::{Pass, Technique};
        let key = (format!("xex:{label}"), String::new());
        if let Some(p) = self.programs.get(&key) {
            return p.clone();
        }
        let raw = self.raw_output;
        let built = (|| {
            let mut sh = vec![Arc::unwrap_or_clone(post::load_xex_shader(dir, vs)?)];
            if let Some(ps) = ps {
                sh.push(Arc::unwrap_or_clone(post::load_xex_shader(dir, ps)?));
            }
            let pass = Pass { name: "p0".into(), vs: Some(0), ps: ps.map(|_| 1), render_states: render_states.to_vec() };
            let fx = Effect { hash: vs, shaders: sh, techniques: vec![Technique { name: "xex".into(), passes: vec![pass] }] };
            globals.learn(&fx);
            let mut program = program::build(&fx, "xex", false, raw, &program::Family::Track)?;
            patch(&mut program)?;
            let handle = shaders.add(Shader::from_wgsl(program.wgsl.clone(), format!("fh1/fx/xex_{label}.wgsl")));
            Some((material::register(&program, handle), Arc::new(program)))
        })();
        if built.is_none() {
            warn!("fh1-render: no xex program {label}");
        }
        self.programs.insert(key, built.clone());
        built
    }

    /// The program for (effect, technique) in a family (track or car); `globals` is that family's
    /// bank.
    pub fn program_family(
        &mut self,
        name: &str,
        technique: &str,
        family: &program::Family,
        shaders: &mut Assets<Shader>,
        globals: &mut FxGlobals,
    ) -> Option<(u32, Arc<Program>)> {
        let tag = match family {
            program::Family::Track => "",
            program::Family::Car { .. } => "#car",
        };
        let key = (name.to_ascii_lowercase(), format!("{technique}{tag}"));
        if let Some(p) = self.programs.get(&key) {
            return p.clone();
        }
        let raw = self.raw_output;
        let built = self.effects.get(&key.0).cloned().and_then(|fx| {
            globals.learn(&fx);
            let program = program::build(&fx, technique, true, raw, family)?;
            let handle = shaders.add(Shader::from_wgsl(program.wgsl.clone(), format!("fh1/fx/{}_{}{tag}.wgsl", key.0, technique)));
            let id = material::register(&program, handle);
            Some((id, Arc::new(program)))
        });
        if built.is_none() {
            warn!("fh1-render: no program for {name} / {technique}");
        }
        self.programs.insert(key, built.clone());
        built
    }

    /// A material for `program` with the model's constants (`vs` → VS c3.., `ps` → PS c0..).
    /// Textures are left unbound (white); fill them with `FxMaterial::slot_mut`.
    pub fn material(&self, program: (u32, &Program), vs: &[[f32; 4]], ps: &[[f32; 4]], globals: &FxGlobals) -> FxMaterial {
        let (id, p) = program;
        let mut consts = FxMaterialConsts::default();
        for (&(stage, reg), v) in &p.material_defaults {
            let file = if stage == Stage::Vertex { &mut consts.vs } else { &mut consts.ps };
            file[reg as usize] = Vec4::from_array(*v);
        }
        // Per-mesh registers baked into the vertex data: identity transforms.
        consts.vs[0] = Vec4::new(0.0, 0.0, 1.0, 1.0); // uvOffsetScale
        consts.vs[1] = Vec4::ONE; // positionScale
        consts.vs[2] = Vec4::ZERO; // positionOffset
        for (k, v) in vs.iter().enumerate() {
            if let Some(r) = consts.vs.get_mut(3 + k) {
                *r = Vec4::from_array(*v);
            }
        }
        for (k, v) in ps.iter().enumerate() {
            if let Some(r) = consts.ps.get_mut(k) {
                *r = Vec4::from_array(*v);
            }
        }
        FxMaterial {
            consts,
            globals: globals.buffer(),
            t0: None,
            t1: None,
            t2: None,
            t3: None,
            t4: None,
            t5: None,
            t6: None,
            t7: None,
            t8: None,
            t9: None,
            t10: None,
            t11: None,
            t12: None,
            t13: None,
            t14: None,
            t15: None,
            cube0: None,
            cube1: None,
            cube2: None,
            headlights: headlight::HEADLIGHT_BUFFER,
            program: id,
            flip_cull: false,
            no_cull: true,
            alpha_blend: p.state.blend.is_some(),
        }
    }
}

/// `FH1_RENDERER=remaster` (mirror of `fh1_remaster::enabled()`; fh1-render can't depend on that crate).
pub fn remaster() -> bool {
    std::env::var("FH1_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("remaster"))
}

static OUTPUT_GAIN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);
/// Gain on the linear colour the game's shaders write while the FH1 post chain is off (raw_output false). The remaster
/// sets its game-curve calibration (fh1_remaster::post::game_unit_scale) so game-unit output matches its exposure.
pub fn set_output_gain(g: f32) {
    OUTPUT_GAIN.store(g.to_bits(), std::sync::atomic::Ordering::Relaxed);
}
pub fn output_gain() -> f32 {
    f32::from_bits(OUTPUT_GAIN.load(std::sync::atomic::Ordering::Relaxed))
}
