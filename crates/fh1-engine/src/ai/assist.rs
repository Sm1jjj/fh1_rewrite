//! Player assists that follow an event's racing line (docs/ASSISTS.md "Driving line / Assisted braking / steering"):
//! the line ahead coloured by braking zones, and the auto-brake / auto-steer halves of FH1's Assisted braking and
//! Assisted steering. A [`Driver`] at the top skill's numbers (AISkills 1 = Max braking / cornering) runs on the player's
//! car in assist-only mode (no resets, no reversing) and supplies the target speeds and the steering.
//!
//! Our rules (the game's assist code is not decoded): braking zone colour from the ratio of the player's speed to the
//! target speed at each point (green below 0.95, yellow at 1.0, red from 1.2); assisted braking brakes as the AI would
//! when over the target speed; assisted steering blends the player's input 60% towards the AI's where the player isn't
//! steering hard.

use bevy::math::Vec3;

use super::driver::{Driver, Situation};
use super::line::RacingLine;
use super::tables::{DriverParams, Skill};
use crate::vehicle::{Controls, Vehicle};

/// One drawn point of the driving line.
#[derive(Debug, Clone, Copy)]
pub struct LinePoint {
    /// On the racing line (engine space, road height of the line data).
    pub position: Vec3,
    /// Unit vector to the left of travel.
    pub left: Vec3,
    /// Player speed / target speed there (1 = right at the limit).
    pub ratio: f32,
}

pub struct PlayerLine {
    pub route: u32,
    pub car: String,
    driver: Driver,
    ai: Controls,
}

impl PlayerLine {
    pub fn new(route: u32, line: &RacingLine, v: &Vehicle) -> Self {
        let mut params = DriverParams::default();
        params.skill = Skill { braking: [1.0, 1.0], cornering: [1.25, 1.25], ..Skill::default() };
        let mut driver = Driver::with_margin(line, v, params, route, super::driver::MARGIN);
        driver.assist_only = true;
        Self { route, car: v.data.media_name.clone(), driver, ai: Controls::default() }
    }

    /// Track the player's car (once per physics tick, before the controls are used).
    pub fn update(&mut self, v: &mut Vehicle, dt: f32) {
        self.ai = self.driver.update(v, Situation::default(), dt).controls;
    }

    /// The player's controls with Assisted braking / steering applied.
    pub fn apply(&self, mut c: Controls, braking: bool, steering: bool) -> Controls {
        if braking && self.ai.brake > 0.0 {
            c.brake = c.brake.max(self.ai.brake);
            c.throttle = 0.0;
        }
        if steering {
            let w = 0.6 * (1.0 - c.steer.abs());
            c.steer = (c.steer + w * (self.ai.steer - c.steer)).clamp(-1.0, 1.0);
        }
        c
    }

    /// Distance along the line and the player's progress (m).
    pub fn s(&self) -> f32 {
        self.driver.proj.s
    }

    pub fn progress(&self) -> f64 {
        self.driver.progress
    }

    /// Points from `from` to `to` metres ahead of the car, every `step` m, with braking-zone ratios for `speed` (m/s).
    pub fn points(&self, speed: f32, from: f32, to: f32, step: f32) -> Vec<LinePoint> {
        let line = &self.driver.line;
        let s0 = self.driver.proj.s;
        let mut out = Vec::new();
        let mut d = from;
        while d <= to {
            let s = s0 + d;
            if !line.closed && s > line.length {
                break;
            }
            let (_, lat, _) = line.road_at(s);
            out.push(LinePoint { position: line.point_at(s), left: lat.normalize_or_zero(), ratio: speed / self.driver.v_max_at(s).max(1.0) });
            d += step;
        }
        out
    }
}

/// Driving line colour for a speed ratio (linear RGB): green -> yellow -> red.
pub fn ratio_colour(ratio: f32) -> [f32; 3] {
    let t = ((ratio - 0.95) / 0.25).clamp(0.0, 1.0);
    if t < 0.2 {
        [0.05, 0.7, 0.1]
    } else if t < 0.6 {
        let k = (t - 0.2) / 0.4;
        [0.05 + 0.85 * k, 0.7, 0.1 * (1.0 - k)]
    } else {
        let k = (t - 0.6) / 0.4;
        [0.9, 0.7 * (1.0 - k) + 0.05 * k, 0.0]
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The game's driving line (CRaceLineRenderer; colours / alpha from 0x825A1CB0, quad from 0x825A00F8, VERIFIED; docs/AI.md
// "Driving line"): a strip along the racing line's waypoints from 12 behind to 102 ahead of the car, one textured
// segment (ui\textures\raceline.xds, a chevron) per two waypoints, 1.2 m wide, 5 cm above the line along its normal,
// drawn by a pixel shader that is just texture.rrra x vertex colour.

/// Waypoints drawn behind / ahead of the car's waypoint, and waypoints per textured segment.
pub const STRIP_BEHIND: isize = 12;
pub const STRIP_AHEAD: isize = 102;
pub const STRIP_STEP: isize = 2;
/// Half width (m) and lift above the line (m) of the strip.
pub const STRIP_HALF_WIDTH: f32 = 0.6;
pub const STRIP_LIFT: f32 = 0.05;

/// One waypoint of the strip.
#[derive(Debug, Clone, Copy)]
pub struct StripPoint {
    /// Waypoint index on the line (world-anchored: the strip never slides with the car).
    pub index: usize,
    pub position: Vec3,
    /// Unit vector to the left of travel.
    pub left: Vec3,
    /// sRGB colour and alpha, 0..1.
    pub rgba: [f32; 4],
}

/// The game's line colour (sRGB bytes R, G, B and an alpha scale 0..255) for `d` = target speed there minus the car's
/// speed (m/s): yellow (192,164,52) around 0, towards green (44,147,44) where the target is higher, towards red
/// (192,39,52) once the car is more than 1 m/s too fast; full effect at 5 m/s. "Braking only" keeps the yellow and fades
/// the green parts out.
pub fn game_line_colour(d: f32, braking_only: bool) -> ([u8; 3], u8) {
    let x = (d.abs() * 0.2).clamp(0.0, 1.0);
    if d <= -1.0 {
        ([192, (164.0 - 125.0 * x) as u8, 52], 255)
    } else if d > 0.0 {
        if braking_only {
            ([192, 164, 52], (255.0 - 255.0 * x) as u8)
        } else {
            ([(192.0 - 148.0 * x) as u8, (164.0 - 17.0 * x) as u8, (52.0 - 8.0 * x) as u8], 255)
        }
    } else {
        ([192, 164, 52], 255)
    }
}

/// The game's alpha (0..255) at `u` waypoints from the strip's first point (12 behind the car): 0 -> 32 over the
/// waypoints behind the car, 32 for 4 ahead, up to 255 by 10 ahead, full to 82 ahead, out by 90 ahead.
pub fn game_line_alpha(u: f32) -> f32 {
    if u < 12.0 {
        (u * (32.0 / 12.0)).clamp(0.0, 32.0)
    } else if u < 16.0 {
        32.0
    } else if u < 22.0 {
        ((u - 16.0) * 37.166_668 + 32.0).max(32.0)
    } else {
        ((102.0 - u) * 12.75).clamp(0.0, 255.0)
    }
}

impl PlayerLine {
    /// The game's strip points for a car at `speed` (m/s): every second waypoint (even indices, so the chevrons stay put
    /// on the road) from 12 behind to 102 ahead, coloured against the assist's target speed at each.
    pub fn strip(&self, speed: f32, braking_only: bool) -> Vec<StripPoint> {
        let line = &self.driver.line;
        let n = line.len() as isize;
        let p = self.driver.proj;
        let car = p.index as isize;
        let start = car - STRIP_BEHIND;
        let mut out = Vec::with_capacity(((STRIP_BEHIND + STRIP_AHEAD) / STRIP_STEP + 2) as usize);
        let mut k = start - start.rem_euclid(STRIP_STEP);
        while k <= car + STRIP_AHEAD {
            if line.closed || (0..n).contains(&k) {
                let i = line.wrap(k);
                let u = (k - start) as f32 - p.t;
                let a = game_line_alpha(u);
                if a > 0.0 {
                    let d = self.driver.v_max_at(line.s[i]) - speed;
                    let ([r, g, b], scale) = game_line_colour(d, braking_only);
                    let alpha = a * scale as f32 / 256.0 / 255.0;
                    out.push(StripPoint {
                        index: i,
                        position: line.points[i],
                        left: line.lateral[i].normalize_or_zero(),
                        rgba: [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, alpha],
                    });
                }
            }
            k += STRIP_STEP;
        }
        out
    }
}
