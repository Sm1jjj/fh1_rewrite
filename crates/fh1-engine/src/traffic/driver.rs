//! A traffic car's driver (docs/TRAFFIC.md "Driving"): follows its lane through the network (random turn at each
//! junction), cruises at the road's density speed, slows for bends, keeps its distance to whatever is ahead (IDM car
//! following) and stops behind the yield point of a busy junction. Two outputs: [`TrafficDriver::controls`] for a car
//! running the full vehicle sim near the player, and [`TrafficDriver::kinematic`] for the cheap far mode (the car slides
//! along its lane; no physics). The game's own traffic driver is not decoded (OUR rules, INFERRED).

use bevy::math::Vec3;

use super::network::{Lane, Network};
use crate::vehicle::{Controls, Vehicle};

/// Comfortable lateral acceleration in bends (m/s², OUR rule).
const LAT_ACCEL: f32 = 2.6;
/// Comfortable deceleration when planning ahead (m/s²).
const PLAN_DECEL: f32 = 2.2;
/// IDM parameters: max acceleration, comfortable braking, standstill gap (bumper to bumper), time headway.
const IDM_A: f32 = 1.6;
const IDM_B: f32 = 2.5;
const IDM_S0: f32 = 3.0;
const IDM_T: f32 = 1.3;

pub fn mph(v: f32) -> f32 {
    v * 0.44704
}

#[derive(Debug, Clone)]
pub struct TrafficDriver {
    pub lane: u32,
    pub s: f32,
    /// Upcoming lanes after `lane`, chosen in advance.
    pub plan: Vec<u32>,
    /// Personal cruise factor (0.9-1.1) on the road's speed.
    pub temper: f32,
    pub speed: f32,
    /// Road traffic: keep to lanes open to road traffic where the junction allows.
    pub road_only: bool,
    rng: u32,
}

/// What lies ahead of the car this tick, from the plugin's obstacle search.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ahead {
    /// Bumper-to-bumper gap (m) and the leader's speed along our path (m/s).
    pub leader: Option<(f32, f32)>,
}

impl TrafficDriver {
    /// A driver for road traffic (`road_only`) or a festival driver.
    pub fn with_kind(net: &Network, lane: u32, s: f32, seed: u32, road_only: bool) -> Self {
        let mut d = Self::new(net, lane, s, seed);
        d.road_only = road_only;
        d.plan.clear();
        d.fill_plan(net);
        d
    }

    pub fn new(net: &Network, lane: u32, s: f32, seed: u32) -> Self {
        let mut d = Self { lane, s, plan: Vec::new(), temper: 1.0, speed: 0.0, road_only: false, rng: seed | 1 };
        d.temper = 0.9 + 0.2 * d.rand();
        d.fill_plan(net);
        d
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    fn fill_plan(&mut self, net: &Network) {
        while self.plan.len() < 3 {
            let last = *self.plan.last().unwrap_or(&self.lane);
            let all = &net.lanes[last as usize].next;
            let open: Vec<u32> = all.iter().copied().filter(|&n| net.lanes[n as usize].road_traffic).collect();
            let next = if self.road_only && !open.is_empty() { &open } else { all };
            if next.is_empty() {
                break;
            }
            let k = ((self.rand() * next.len() as f32) as usize).min(next.len() - 1);
            self.plan.push(next[k]);
        }
    }

    pub fn current<'a>(&self, net: &'a Network) -> &'a Lane {
        &net.lanes[self.lane as usize]
    }

    /// Move `ds` metres along the path (rolling onto the planned lanes). False at a dead end without a plan.
    pub fn advance(&mut self, net: &Network, ds: f32) -> bool {
        self.s += ds;
        while self.s > self.current(net).length {
            if self.plan.is_empty() {
                self.s = self.current(net).length;
                return false;
            }
            self.s -= self.current(net).length;
            self.lane = self.plan.remove(0);
            self.fill_plan(net);
        }
        true
    }

    /// Point and tangent `d` metres ahead along the path.
    pub fn ahead(&self, net: &Network, d: f32) -> (Vec3, Vec3) {
        let mut s = self.s + d;
        let mut lane = self.current(net);
        for &l in &self.plan {
            if s <= lane.length {
                break;
            }
            s -= lane.length;
            lane = &net.lanes[l as usize];
        }
        lane.at(s)
    }

    /// Distance to the end of the current lane and the node there.
    pub fn to_lane_end(&self, net: &Network) -> (f32, u32) {
        let l = self.current(net);
        (l.length - self.s, l.to)
    }

    /// Follow a physically simulated car: re-project its position onto the path (rolls over lanes as it drives on).
    /// Returns the lateral distance from the lane.
    pub fn sync(&mut self, net: &Network, p: Vec3) -> f32 {
        let (s, d) = self.current(net).project(p, self.s);
        if s >= self.current(net).length - 0.5 && !self.plan.is_empty() {
            let next = &net.lanes[self.plan[0] as usize];
            let (s2, d2) = next.project(p, 0.0);
            if d2 <= d + 0.5 && s2 > 0.0 {
                self.lane = self.plan.remove(0);
                self.fill_plan(net);
                self.s = s2;
                return d2;
            }
        }
        self.s = s;
        d
    }

    /// The speed the car wants now: road speed × temper, limited by the bends ahead (planned at PLAN_DECEL).
    pub fn target_speed(&self, net: &Network, cruise: f32) -> f32 {
        let v0 = cruise * self.temper;
        let horizon = (v0 * v0 / (2.0 * PLAN_DECEL) + 25.0).min(160.0);
        let mut want = v0;
        let mut d = 0.0;
        let (_, mut prev_t) = self.ahead(net, 0.0);
        while d < horizon {
            d += 5.0;
            let (_, t) = self.ahead(net, d);
            let turn = prev_t.angle_between(t);
            prev_t = t;
            if turn < 0.02 {
                continue;
            }
            // Heading change over 5 m -> curvature.
            let k = turn / 5.0;
            let v_bend = (LAT_ACCEL / k).sqrt().max(4.0);
            let v_here = (v_bend * v_bend + 2.0 * PLAN_DECEL * (d - 5.0).max(0.0)).sqrt();
            want = want.min(v_here);
        }
        want
    }

    /// IDM acceleration towards `v0` with an optional leader (gap, leader speed).
    pub fn idm(v: f32, v0: f32, leader: Option<(f32, f32)>) -> f32 {
        let v0 = v0.max(0.1);
        let free = 1.0 - (v / v0).powi(4);
        let inter = match leader {
            Some((gap, vl)) => {
                let dv = v - vl;
                let s_star = IDM_S0 + (v * IDM_T + v * dv / (2.0 * (IDM_A * IDM_B).sqrt())).max(0.0);
                (s_star / gap.max(0.3)).powi(2)
            }
            None => 0.0,
        };
        (IDM_A * (free - inter)).clamp(-9.0, IDM_A)
    }

    /// Full-sim mode: steering by pure pursuit on the path, throttle / brake from the wanted acceleration.
    pub fn controls(&mut self, net: &Network, v: &mut Vehicle, accel: f32) -> Controls {
        let speed = v.forward_speed();
        self.speed = speed.max(0.0);
        let look = (4.0 + 0.45 * speed.abs()).clamp(5.0, 30.0);
        let (target, _) = self.ahead(net, look);
        let fwd = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
        let right = fwd.cross(Vec3::Y);
        let to = target - v.position;
        let to = Vec3::new(to.x, 0.0, to.z);
        let ld = to.length().max(1.0);
        let sin_a = to.dot(right) / ld;
        let wheelbase = (v.data.hubs[2][2] - v.data.hubs[0][2]).abs().max(2.0);
        let mut angle = (2.0 * wheelbase * sin_a / ld).atan();
        // Cross-track correction (Stanley term): steer back toward the lane by the sideways error, softer with speed.
        // Pure pursuit alone cut the insides of bends and never pulled a car that had drifted back onto its lane (user
        // 2026-10-07: "they tend to drive off their line"). FH1_TRAFFIC_XTRACK=<gain> (0 = off; default 0.8).
        let gain = xtrack_gain();
        if gain > 0.0 {
            let (p0, t0) = self.ahead(net, 0.0);
            let lane_right = t0.reject_from(Vec3::Y).normalize_or(fwd).cross(Vec3::Y);
            let e = (v.position - p0).dot(lane_right);
            angle -= (gain * e / (speed.abs() + 4.0)).atan();
        }
        let lock = v.steer_lock_at(speed.abs()).max(0.05);
        let steer = (angle / lock).clamp(-1.0, 1.0);
        let (throttle, brake) = if accel >= 0.0 {
            // Hold speed: a little throttle to cancel drag, more to accelerate.
            ((0.12 + accel / 3.0).clamp(0.0, 0.75), 0.0)
        } else if accel > -0.6 {
            (0.0, 0.0)
        } else {
            (0.0, ((-accel - 0.6) / 7.0).clamp(0.05, 1.0))
        };
        // Standstill: hold the brake so the car doesn't creep.
        let (throttle, brake) = if accel < 0.0 && speed < 0.5 { (0.0, 0.4) } else { (throttle, brake) };
        Controls { steer, throttle, brake, handbrake: 0.0, tcs: true, abs: true, ..Default::default() }
    }

    /// Far mode: integrate speed along the lane. Returns (position, tangent) on the lane.
    pub fn kinematic(&mut self, net: &Network, accel: f32, dt: f32) -> (Vec3, Vec3) {
        self.speed = (self.speed + accel * dt).max(0.0);
        if !self.advance(net, self.speed * dt) {
            self.speed = 0.0;
        }
        self.ahead(net, 0.0)
    }
}

/// Engine rpm and gear for a car moving at `v` without its drivetrain sim (far mode; audio reads them): the highest gear
/// that keeps the engine above ~1.35× idle, capped below 0.7 of the redline.
pub fn cruise_rpm(v: &Vehicle, speed: f32) -> (usize, f32) {
    let d = &v.data;
    let wheel_rpm = speed / d.tyre_radius[1].max(0.2) * 60.0 / std::f32::consts::TAU;
    let floor = d.idle_rpm * 1.35;
    let mut pick = (1, d.idle_rpm);
    for (i, &g) in d.gears.iter().enumerate() {
        let rpm = wheel_rpm * g * d.final_drive;
        if i == 0 || rpm >= floor {
            pick = (i + 1, rpm.max(d.idle_rpm));
        }
        if rpm < floor {
            break;
        }
    }
    (pick.0, pick.1.min(d.redline_rpm * 0.7))
}

/// `FH1_TRAFFIC_DIRECT_STEER=0`: traffic steers through the player's mode-0 controller (before 2026-10-08) instead of
/// `Vehicle::direct_steer` (input x speed lock, no zero-crossing jump to input x SteerMaxAngle).
pub fn direct_steer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_TRAFFIC_DIRECT_STEER").map_or(true, |v| v != "0"))
}

/// Cross-track steering gain (see `controls`); 0 = pure pursuit only (before 2026-10-07).
fn xtrack_gain() -> f32 {
    static G: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *G.get_or_init(|| std::env::var("FH1_TRAFFIC_XTRACK").ok().and_then(|v| v.parse().ok()).unwrap_or(0.8))
}
