//! FH1's driving HUD (`947_HUD`): the analog tach (hatches, numbers, needle, redline), the digital
//! speedo, gear, assist icons, shift light and the radio pop-up, driven from the player's car.
//!
//! Free roam shows GAUGE_ANALOG, the minimap and the radio element; the race / career widgets
//! (place, laps, timers, leaderboard, skills, popularity…) are hidden until those modes exist.
//! The gauge pieces are code-driven Anark components: each hatch / number / needle is positioned
//! by seeking its timeline (`Player::set_progress`), as the game's ANALOG_GAUGE / PROGRESS_BAR
//! contracts do (docs/UI.md).

use bevy::prelude::*;
use fh1_radio::system::HudPost;
use fh1_ui::player::{Player, Value};

use super::scene::{AnarkScene, UiData};
use super::{Menu, Settings};
use crate::camera::CameraRig;
use crate::Car;

/// A radio HUD post that has come due (`radio.rs` applies the post's delay, then forwards it).
#[derive(Message, Clone)]
pub struct RadioHudPost(pub HudPost);

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<RadioHudPost>()
            .add_systems(Update, (drive_hud, drive_radio).chain().after(crate::sync_visuals))
            .add_systems(PostUpdate, drive_mirror);
    }
}

/// Race / career widgets hidden in free roam (STOPGAP until those modes exist).
const HIDDEN: &[&str] = &[
    "Mirror", "WrongWay", "EndConditionBlock", "PlaceBlock", "DrivingSkill", "DirectionArrows",
    "PopularityBar", "PopularityUnlock", "HUDTimes", "PositionIndicators", "ActivationPrompt",
    "RaceEncounter", "Missions", "SpeedCamera", "TicketCountdown", "EndOfRaceCountdown", "E3RacePlace", "I_321_Go",
    "RadioTuning", "AutosaveIcon", "SatNavCommands",
];

#[derive(Resource)]
pub struct Fh1Hud {
    entity: Entity,
    o: Objects,
    labels: Labels,
    shown: bool,
    last_gear: usize,
    /// Radio pop-up: when the current CHANGESTATION / CHANGETRACK slide ends, HIDE_INFO.
    radio_open: bool,
    /// The satnav Details block (distance readout) is on its Show slide.
    target_shown: bool,
    /// `I_HUD_Mirror` rect in UI pixels (centre from the screen centre, y up; size), taken from the scene's default
    /// state at load (the element itself stays hidden: it draws nothing, the game's CUSTREND_MIRRORRENDERER does).
    mirror_rect: Option<(Vec2, Vec2)>,
    /// Player::reset count: a reset drops every override and slide, so the objective line re-syncs (ui/notify.rs).
    pub(super) resets: u32,
    /// World generation the HUD was set up for: an in-process map change redoes the setup (minimap / satnav shown on
    /// Colorado only, X1d).
    generation: u32,
}

impl Fh1Hud {
    /// The `947_HUD` AnarkScene entity: race (race/anark_hud.rs, fa) and skill (ui/skillhud.rs, e4) widgets are driven
    /// by their owners in systems after `drive_hud`.
    pub fn scene_entity(&self) -> Entity {
        self.entity
    }

    /// Bumped on every `Player::reset` (which drops overrides and re-hides the HIDDEN widgets): owners re-apply then.
    pub fn reset_count(&self) -> u32 {
        self.resets
    }
}

struct Labels {
    kmh: String,
    mph: String,
    reverse: String,
}

#[derive(Default)]
struct Objects {
    needle: Option<usize>,
    redline: Option<usize>,
    fuel: Option<usize>,
    large: Vec<usize>,
    mid: Vec<usize>,
    small: Vec<usize>,
    numbers: Vec<(usize, Option<usize>)>,
    gear: Option<usize>,
    unit: Option<usize>,
    digits: [Option<usize>; 3],
    abs: Option<usize>,
    stm: Option<usize>,
    tcs: Option<usize>,
    shift: Option<usize>,
    swoosh_up: Option<usize>,
    swoosh_down: Option<usize>,
    hidden: Vec<usize>,
    radio: Option<usize>,
    radio_element: Option<usize>,
    radio_logos: Option<usize>,
    radio_track: Option<usize>,
    radio_artist: Option<usize>,
    minimap: Option<usize>,
    north_arrow: Option<usize>,
    map_quad: Option<usize>,
    /// MiniMap/Details: the satnav distance readout (icon, icon2, TEXT_DISTANCE).
    details: Option<usize>,
    distance: Option<usize>,
}

impl Objects {
    fn find(p: &Player) -> Self {
        let f = |n: &str| p.find(n);
        let comps = |parent: Option<usize>| parent.map(|g| p.children(g).to_vec()).unwrap_or_default();
        let gauge = f("RPM_ANALOG");
        let under = |root: Option<usize>, name: &str| root.and_then(|r| find_below(p, r, name));
        let numbers_node = under(gauge, "Numbers");
        Self {
            needle: f("_P_Tach_Needle"),
            redline: f("Redline"),
            fuel: f("FUEL_ANALOG"),
            large: comps(under(gauge, "Large_Hatches")),
            mid: comps(under(gauge, "Mid_Hatches")),
            small: comps(under(gauge, "Small_Hatches")),
            numbers: comps(numbers_node).into_iter().map(|c| (c, p.resolve_path(c, "rotation.Text"))).collect(),
            gear: f("TEXT_GEAR_ANALOG"),
            unit: f("TEXT_SPEED_UNIT_ANALOG"),
            digits: [f("digit0"), f("digit1"), f("digit2")],
            abs: f("HUD_ABS"),
            stm: f("HUD_STM"),
            tcs: f("HUD_TCS"),
            shift: f("HUD_SHIFT_LIGHT"),
            swoosh_up: f("Up"),
            swoosh_down: f("Down"),
            hidden: HIDDEN.iter().filter_map(|n| f(n)).collect(),
            radio: f("RadioStation"),
            radio_element: f("CompleteElement"),
            radio_logos: f("RSLogos"),
            radio_track: f("RadioStation").and_then(|r| p.resolve_path(r, "CompleteElement.Content.Track.Text")),
            radio_artist: f("RadioStation").and_then(|r| p.resolve_path(r, "CompleteElement.Content.Artist.Text")),
            minimap: f("MiniMap"),
            north_arrow: f("NorthArrow"),
            map_quad: f("MATERIAL_VIEWPORT"),
            details: f("Details"),
            distance: f("TEXT_DISTANCE"),
        }
    }
}

/// First descendant of `from` with this name.
fn find_below(p: &Player, from: usize, name: &str) -> Option<usize> {
    let mut stack = vec![from];
    while let Some(o) = stack.pop() {
        for &c in p.children(o) {
            if p.name(c) == Some(name) {
                return Some(c);
            }
            stack.push(c);
        }
    }
    None
}

/// Load `947_HUD` (called from `ui::load_fh1_ui`). `minimap` is the map render target for the
/// scene's placeholder texture.
pub fn spawn(commands: &mut Commands, data: &UiData, minimap: Option<Handle<Image>>) {
    let Some(p) = data.scene("947_HUD") else { return };
    let text = |r: &str, d: &str| data.strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| d.to_owned());
    let o = Objects::find(&p);
    let labels = Labels {
        // The scene authors "mph" in lower case and the refs show it so; the km/h case is a GUESS.
        kmh: text("Activities:IDS_KmPerHour", "KM/H").to_lowercase(),
        mph: text("Activities:IDS_Mph", "MPH").to_lowercase(),
        reverse: text("InGame:IDS_ReverseGear", "R"),
    };
    let mut p = p;
    let mirror_rect = mirror_rect(&mut p);
    let mut sc = AnarkScene::new(p, 10);
    sc.visible = false;
    sc.text_scale = super::scene::TEXT_PT_TO_PX;
    if let Some(img) = minimap {
        sc.texture_overrides.insert("horizon/placeholder.png".into(), img);
    }
    let entity = commands.spawn(sc).id();
    commands.insert_resource(Fh1Hud { entity, o, labels, shown: false, last_gear: 1, radio_open: false, target_shown: false, mirror_rect, resets: 0, generation: 0 });
}

/// The tach layout, as the game's ANALOG_GAUGE range function (default.xex 0x827D9018). The top is
/// T = ceil(max rpm / 1000) thousands; the label step grows (1, 2, 3…) until the intervals are fewer
/// than the gauge's label count. The HUD gauge takes the "odd" branch when T is odd: the intervals
/// are counted as T/step + 1/2 (floored) and the last one carries no end label or end hatch (the
/// Corrado, T = 9: labels 0-8 on a 0-9000 dial, as the Xenia refs show).
/// Max rpm = TorqueCurveMaxRPM (INFERRED from the Corrado: 8500 gives the refs' 9000 dial).
struct Dial {
    intervals: usize,
    step: u32,
    odd: bool,
}

impl Dial {
    fn new(max_rpm: f32, labels: usize) -> Self {
        let top = (max_rpm / 1000.0).ceil().max(1.0);
        // The gauge's odd-top flag (GUESS: set for the HUD tach; only the odd case is checked).
        let odd = top as u32 % 2 == 1;
        let half = if odd { 0.5 } else { 0.0 };
        let mut step = 1u32;
        let mut count = top / step as f32 + half;
        while count >= labels.max(2) as f32 {
            step += 1;
            count = (top / step as f32 + half).ceil();
        }
        Self { intervals: count.floor().max(1.0) as usize, step, odd }
    }

    /// The rpm at the dial's end.
    fn max(&self) -> f32 {
        (self.intervals as u32 * self.step) as f32 * 1000.0
    }

    /// Labels and large hatches (one fewer in the odd case: no end label).
    fn labels(&self) -> usize {
        if self.odd { self.intervals } else { self.intervals + 1 }
    }

    /// Small hatches at the quarter marks.
    fn smalls(&self) -> usize {
        if self.odd { 2 * self.intervals - 1 } else { 2 * self.intervals }
    }
}

fn redline_ceil() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_HUD_REDLINE_CEIL").is_ok_and(|v| v == "1"))
}

fn shift_light_guess() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SHIFT_LIGHT").is_ok_and(|v| v == "1"))
}

fn hide(p: &mut Player, o: usize) {
    p.set(o, fh1_ui::names::OPACITY, Value::Float(0.0));
}

fn show(p: &mut Player, o: usize) {
    p.set(o, fh1_ui::names::OPACITY, Value::Float(100.0));
}

/// Seek a code-driven component to a fraction of its slide and hold it there.
fn seek(p: &mut Player, c: usize, f: f32) {
    p.set_progress(c, f);
    p.set_playing(c, false);
}

#[allow(clippy::too_many_arguments)]
pub fn drive_hud(
    hud: Option<ResMut<Fh1Hud>>,
    mut scenes: Query<&mut AnarkScene>,
    cars: Query<&Car>,
    settings: Res<Settings>,
    menu: Res<Menu>,
    rig: Res<CameraRig>,
    satnav: Option<Res<super::minimap::SatNav>>,
    track: Res<crate::track::Track>,
    generation: Res<super::world_load::WorldGeneration>,
) {
    let Some(mut hud) = hud else { return };
    let hud = &mut *hud;
    if hud.generation != generation.0 {
        hud.generation = generation.0;
        hud.shown = false;
        hud.target_shown = false;
    }
    let Ok(mut sc) = scenes.get_mut(hud.entity) else { return };
    let Ok(car) = cars.single() else { return };
    let want = settings.hud && !menu.open && !rig.photo && !crate::cutscene::hide_hud();
    sc.visible = want;
    if !want {
        return;
    }
    let v = &car.0;
    let o = &hud.o;
    let p = &mut sc.player;
    if !hud.shown {
        p.reset();
        hud.resets += 1;
        for &h in &o.hidden {
            hide(p, h);
        }
        if let Some(r) = o.radio_element {
            p.goto_slide(r, "OFF");
        }
        if let Some(f) = o.fuel {
            // FH1 has no fuel; the gauge piece reads full.
            seek(p, f, 1.0);
        }
        for &s in [o.swoosh_up, o.swoosh_down].iter().flatten() {
            hide(p, s);
        }
        place_north_arrow(p, o);
        // The minimap and its satnav readout draw Colorado's road network: none on other maps (an empty disc).
        if track.id != "colorado" {
            // MATERIAL_VIEWPORT (the map disc) is not below MiniMap.
            for &m in [o.minimap, o.map_quad, o.details].iter().flatten() {
                hide(p, m);
            }
        }
    }
    // North arrow: heading-up map, so the arrow turns by the car's heading (positive z rotation is
    // counter-clockwise on screen; heading measured clockwise from north = engine −Z).
    if let Some(na) = o.north_arrow {
        let fwd = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
        let heading = fwd.x.atan2(-fwd.z);
        p.set(na, fh1_ui::names::ROTATION_Z, Value::Float(heading));
    }

    // Dial (0x827D9018): label k at progress k/n reads k·step; large hatches at the labels, mid
    // hatches half-way, small ones at the quarters. Pieces past the dial end are hidden.
    let dial = Dial::new(v.data.torque_curve_max_rpm, o.numbers.len());
    let n = dial.intervals as f32;
    let place = |list: &[usize], count: usize, first: f32, spacing: f32, p: &mut Player| {
        for (k, &c) in list.iter().enumerate() {
            if k < count {
                show(p, c);
                seek(p, c, (first + spacing * k as f32) / n);
            } else {
                hide(p, c);
            }
        }
    };
    place(&o.large, dial.labels(), 0.0, 1.0, p);
    place(&o.mid, dial.intervals, 0.5, 1.0, p);
    place(&o.small, dial.smalls(), 0.25, 0.5, p);
    let numbers: Vec<usize> = o.numbers.iter().map(|&(c, _)| c).collect();
    place(&numbers, dial.labels(), 0.0, 1.0, p);
    for (k, (_, text)) in o.numbers.iter().enumerate().take(dial.labels()) {
        if let Some(t) = text {
            p.set_text(*t, (k as u32 * dial.step).to_string());
        }
    }
    let max = dial.max();
    if let Some(nd) = o.needle {
        seek(p, nd, (v.rpm / max).clamp(0.0, 1.0));
    }
    // Redline slider (VERIFIED code + Pinyon probe; docs/UI.md "Driving HUD"): the range fn 0x827D9018 stores the
    // engine's redline (vtable +0x84 = engine+452 rad/s x 60/2pi = RedlineRPM, Viper 649.26 rad/s = 6200) unrounded
    // into the slider, whose update 0x82807BE0 seeks (value - min) / (end - min), min = 0.
    // FH1_HUD_REDLINE_CEIL=1 = the old guess (RedlineRPM rounded up to 1000).
    if let Some(r) = o.redline {
        let mark = if redline_ceil() { (v.data.redline_rpm / 1000.0).ceil() * 1000.0 } else { v.data.redline_rpm };
        seek(p, r, (mark / max).clamp(0.0, 1.0));
    }

    // Speed, three digits with leading zeros as the scene authors them; gear; unit.
    let speed = if settings.metric { v.speed() * 3.6 } else { v.speed() * 2.236_94 };
    // SPEEDO (0x8281D8D0): digit k (from the right, place 10^k) shows (speed / 10^k) % 10 at
    // opacity 100, or 25 for a leading zero (speed < 10^k, k > 0).
    let speed = (speed.abs().round() as u32).min(999);
    for (k, d) in o.digits.iter().rev().enumerate() {
        let Some(d) = *d else { continue };
        let place = 10u32.pow(k as u32);
        p.set_text(d, ((speed / place) % 10).to_string());
        let dim = k > 0 && speed < place;
        p.set(d, fh1_ui::names::OPACITY, Value::Float(if dim { 25.0 } else { 100.0 }));
    }
    // Satnav distance under the minimap (one decimal as the Xenia refs' "0.3 MI"; km form GUESS).
    if let (Some(d), Some(t)) = (o.details, o.distance) {
        match satnav.as_ref().and_then(|s| s.distance_m) {
            // Details is slide-driven (MiniMap's SHOW_TARGET / HIDE_TARGET go to Show / Hidden):
            // change the slide only on a change so its animation isn't restarted every frame.
            Some(m) => {
                if !hud.target_shown {
                    p.goto_slide(d, "Show");
                    hud.target_shown = true;
                }
                let (v, unit) = if settings.metric { (m / 1000.0, "KM") } else { (m / 1609.344, "MI") };
                p.set_text(t, format!("{v:.1} {unit}"));
            }
            None => {
                if hud.target_shown {
                    p.goto_slide(d, "Hidden");
                    hud.target_shown = false;
                }
            }
        }
    }
    if let Some(u) = o.unit {
        p.set_text(u, if settings.metric { hud.labels.kmh.clone() } else { hud.labels.mph.clone() });
    }
    // Gear (0x827ED8D8): R for reverse, N for neutral, else the number (the Xenia refs show "1"
    // standing still in first). The sim has no neutral gear.
    let gear = match v.gear {
        0 => hud.labels.reverse.clone(),
        g => g.to_string(),
    };
    if let Some(g) = o.gear {
        p.set_text(g, gear);
    }

    // Assists: Hidden when switched off; Focused while the sim's ABS / TCS / STM intervene this tick
    // (car+0x169C ABS flag, TCS throttle cut, car+0x1698 STM flag), else Blurred.
    for (comp, on, active) in [(o.abs, settings.abs, v.abs_active()), (o.tcs, settings.tcs, v.tcs_cut > 0.0), (o.stm, settings.stm, v.stm_active)] {
        let Some(c) = comp else { continue };
        let want = if !on { "Hidden" } else if active { "Focused" } else { "Blurred" };
        set_state(p, c, want);
    }
    // HUD_SHIFT_LIGHT: 947_HUD drives it with the events SHIFT_LIGHT_ON / _OFF, which default.xex never raises (no
    // such string or ahash31 constant; STM/ABS/TCS_ON/OFF are there). So the game leaves it in its authored state
    // (docs/UI.md "Driving HUD"). FH1_SHIFT_LIGHT=1 = the old guess (lit above 94% of redline). The game's actual shift
    // light logic (CShiftLights, rpm / (0.9 x redline)) belongs to another screen's CONTROL_SHIFT_LIGHTS.
    if let Some(sl) = o.shift.filter(|_| shift_light_guess()) {
        let lit = v.rpm > v.data.redline_rpm * 0.94;
        set_state(p, sl, if lit { "Focus" } else { "Blurred" });
    }

    // Needle swooshes on gear changes (GUESS: Up on upshift, Down on downshift).
    if v.gear != hud.last_gear {
        let up = v.gear > hud.last_gear;
        if let Some(sw) = if up { o.swoosh_up } else { o.swoosh_down } {
            show(p, sw);
            p.goto_slide(sw, "Slide1");
        }
        hud.last_gear = v.gear;
    }
    hud.shown = true;
}

/// The game positions the satnav widget over the map: put the NorthArrow's rotation centre (its
/// `position`, around which it orbits at its pivot distance of 78 px) on the map quad's centre.
fn place_north_arrow(p: &mut Player, o: &Objects) {
    let (Some(mm), Some(na), Some(quad)) = (o.minimap, o.north_arrow, o.map_quad) else { return };
    let f = p.evaluate();
    let (Some(&(wm, lm)), Some(&(wq, lq))) = (f.objects.get(&mm), f.objects.get(&quad)) else { return };
    let (cm, cq) = (f.camera(lm).position, f.camera(lq).position);
    // Screen position of the quad centre, expressed in the MiniMap's layer.
    let target = Vec2::new(wq[0][3] - cq[0] + cm[0], wq[1][3] - cq[1] + cm[1]);
    // Into MiniMap-local coordinates (2D affine inverse).
    let m = Mat2::from_cols(Vec2::new(wm[0][0], wm[1][0]), Vec2::new(wm[0][1], wm[1][1]));
    if m.determinant().abs() < 1e-6 {
        return;
    }
    let local = m.inverse() * (target - Vec2::new(wm[0][3], wm[1][3]));
    p.set(na, fh1_ui::names::POSITION_X, Value::Float(local.x));
    p.set(na, fh1_ui::names::POSITION_Y, Value::Float(local.y));
    // Authored at opacity 0 and left so in free roam: no Xenia free-roam frame shows it (refs
    // ref_festival_a/b/c, l4/x_pos1/x_pos2). When HUD_SAT_NAV turns it on is still open.
}

/// Go to a slide only when it isn't current (so it isn't restarted every frame).
fn set_state(p: &mut Player, component: usize, slide: &str) {
    let current = p.clock(component).map(|c| p.scene.bgf.slides[c.slide].name.as_str() == slide);
    if current != Some(true) {
        p.goto_slide(component, slide);
    }
}

/// Radio pop-up, following the game's widget (docs/UI.md "Radio HUD events").
fn drive_radio(hud: Option<ResMut<Fh1Hud>>, mut scenes: Query<&mut AnarkScene>, mut posts: MessageReader<RadioHudPost>) {
    let Some(mut hud) = hud else {
        posts.clear();
        return;
    };
    let hud = &mut *hud;
    let Ok(mut sc) = scenes.get_mut(hud.entity) else { return };
    let (Some(radio), Some(element)) = (hud.o.radio, hud.o.radio_element) else { return };
    let p = &mut sc.player;
    for RadioHudPost(post) in posts.read() {
        if post.station > 3 {
            continue;
        }
        if let Some(l) = hud.o.radio_logos {
            p.goto_slide(l, ["BASSARENA", "PULSE", "2ELEVEN", "OFF"][post.station]);
        }
        for (text, value, show_ev, hide_ev) in [
            (hud.o.radio_track, &post.title, "SHOW_TRACK_NAME", "HIDE_TRACK_NAME"),
            (hud.o.radio_artist, &post.artist, "SHOW_ARTIST_NAME", "HIDE_ARTIST_NAME"),
        ] {
            if value.is_empty() {
                p.fire_at(hide_ev, radio);
            } else {
                if let Some(t) = text {
                    p.set_text(t, value.clone());
                }
                p.fire_at(show_ev, radio);
            }
        }
        p.fire_at(if post.show_station { "SHOW_STATION" } else { "SHOW_INFO" }, radio);
        hud.radio_open = true;
    }
    // The scene's own START_HIDE / HIDE_COMPLETE come at the end of the slide; then HIDE_INFO.
    if hud.radio_open {
        if let Some(c) = p.clock(element) {
            if !c.playing {
                p.fire_at("HIDE_INFO", radio);
                hud.radio_open = false;
            }
        }
    }
}

/// The HUD mirror's node (`Mirror/I_HUD_Mirror/I_HUD_Mirror`, the CUSTREND_MIRRORRENDERER contract target) as a
/// screen rect. The node's scale is taken as its size in UI pixels (INFERRED: the node has no mesh of its own).
fn mirror_rect(p: &mut Player) -> Option<(Vec2, Vec2)> {
    let outer = p.find("I_HUD_Mirror")?;
    let node = find_below(p, outer, "I_HUD_Mirror").unwrap_or(outer);
    // The element is authored hidden (the game shows it only in some modes): make the chain visible to read it.
    let chain: Vec<usize> = [p.find("Mirror"), Some(outer), Some(node)].into_iter().flatten().collect();
    for &o in &chain {
        show(p, o);
    }
    let f = p.evaluate();
    p.reset();
    let &(w, layer) = f.objects.get(&node)?;
    let cam = f.camera(layer).position;
    let centre = Vec2::new(w[0][3] - cam[0], w[1][3] - cam[1]);
    let size = Vec2::new(Vec2::new(w[0][0], w[1][0]).length(), Vec2::new(w[0][1], w[1][1]).length());
    info!("hud: I_HUD_Mirror at {centre} scale {size}");
    Some((centre, size))
}

/// The HUD rear-view mirror quad, showing fh1_render::mirror's render (mirrored left-right, as a mirror).
#[derive(Component)]
struct HudMirrorQuad;

/// Mirror size in UI pixels when the node scale is a unit scale (GUESS until the Pinyon reference shot).
const MIRROR_PX: Vec2 = Vec2::new(256.0, 64.0);

#[allow(clippy::too_many_arguments)]
fn drive_mirror(
    mut commands: Commands,
    hud: Option<Res<Fh1Hud>>,
    scenes: Query<&AnarkScene>,
    mirror: Option<Res<fh1_render::mirror::MirrorView>>,
    mut quads: Query<&mut Visibility, With<HudMirrorQuad>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut cmats: ResMut<Assets<ColorMaterial>>,
) {
    let (Some(hud), Some(mirror)) = (hud, mirror) else { return };
    let Some((centre, scale)) = hud.mirror_rect else { return };
    let shown = mirror.allowed && mirror.enabled && scenes.get(hud.entity).is_ok_and(|s| s.visible);
    if let Ok(mut vis) = quads.single_mut() {
        let want = if shown { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
        return;
    }
    if !shown {
        return;
    }
    let size = if scale.x > 4.0 && scale.y > 1.0 { scale } else { MIRROR_PX };
    // Mirrored horizontally: u runs right to left.
    let mut mesh = Mesh::from(Rectangle::new(size.x, size.y));
    if let Some(bevy::mesh::VertexAttributeValues::Float32x2(uv)) = mesh.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        for t in uv.iter_mut() {
            t[0] = 1.0 - t[0];
        }
    }
    // In front of the HUD parts (all at z 0).
    let place = (HudMirrorQuad, Transform::from_xyz(centre.x, centre.y, 1.0), bevy::camera::visibility::RenderLayers::layer(super::scene::UI_LAYER));
    if super::scene::hud_2d() {
        // The HUD camera is a Camera2d (scene.rs hud_2d): a 2D mesh with the same unlit texture.
        let mat = cmats.add(ColorMaterial { texture: Some(mirror.image.clone()), ..default() });
        commands.spawn((place, Mesh2d(meshes.add(mesh)), bevy::sprite_render::MeshMaterial2d(mat)));
    } else {
        let mat = mats.add(StandardMaterial { base_color_texture: Some(mirror.image.clone()), unlit: true, fog_enabled: false, alpha_mode: AlphaMode::Opaque, ..default() });
        commands.spawn((place, Mesh3d(meshes.add(mesh)), MeshMaterial3d(mat)));
    }
}
