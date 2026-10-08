//! The traffic lane graph built from `colorado.nav` (fh1_ui::nav; docs/TRAFFIC.md "Road network").
//!
//! Traffic ways = `road_type` a / b / freeway / dirt with a `traffic_density` tag, not `ai_disabled` (`traffic_disabled`
//! ways are kept for festival drivers only).
//! Ways are split into roads at nodes shared with another traffic way (junctions). Each road gives lanes: a two-way road
//! one lane per direction offset to the right of travel (Colorado drives on the right: AIRacing.xml freeroam
//! LegalSideOfRoad="Right"); a `oneway` way runs in node order (VERIFIED: every one of 110 sampled opposing carriageway
//! pairs has the other carriageway on its left) with two lanes on freeways, one elsewhere. Lane widths are OUR guess.
//!
//! Lane fit (2026-10-08, user: "highway traffic drives down the middle of the carriageway"; `FH1_TRAFFIC_LANE_FIT=0` = off):
//! the nav line is not always on the paved centre (test `road_width_survey`: freeway carriageways ~17 m of paved road,
//! centre up to a few metres off the line). With the collision ground, each nav point is re-centred on the paved run
//! across it (on-road surfaces, smoothed along the way) and wide roads get real lanes: one-way carriageways get 3.65 m
//! lanes centred in the run, two-way roads at least 16 m wide (undivided freeway) two lanes per direction.

use std::collections::HashMap;

use bevy::math::Vec3;
use fh1_ui::nav::Nav;

use crate::vehicle::Ground;

/// Lane width for fitted lanes (m, OUR value: a US interstate lane, 12 ft).
const LANE_W: f32 = 3.65;
/// Paved-run scan across a nav point: half width and step (m).
const SCAN_HALF: f32 = 12.0;
const SCAN_STEP: f32 = 0.5;
/// The fit never moves a lane centre further than this from the nav line (m).
const MAX_SHIFT: f32 = 4.0;

fn lane_fit_enabled() -> bool {
    std::env::var("FH1_TRAFFIC_LANE_FIT").map_or(true, |v| v != "0")
}

/// Paved run across `p` (perpendicular `right`): (left, right) edges in metres along `right`, or None if `p` itself
/// isn't on a paved (on-road) surface.
fn paved_run(ground: &dyn Ground, p: Vec3, right: Vec3) -> Option<(f32, f32)> {
    let paved = |x: f32| {
        let q = p + right * x;
        ground.ray(q + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).is_some_and(|h| h.tyre.offroadness <= 0.0 && h.normal.y > 0.7)
    };
    if !paved(0.0) {
        return None;
    }
    let n = (SCAN_HALF / SCAN_STEP) as i32;
    let mut l = 0;
    while l > -n && paved((l - 1) as f32 * SCAN_STEP) {
        l -= 1;
    }
    let mut r = 0;
    while r < n && paved((r + 1) as f32 * SCAN_STEP) {
        r += 1;
    }
    Some((l as f32 * SCAN_STEP, r as f32 * SCAN_STEP))
}

/// Per point of a nav polyline: (paved centre offset, paved width), median-smoothed over 5 points; points without a
/// reading take the nearest reading (none at all: centre 0, width 0 = no fit).
fn fit_line(ground: &dyn Ground, pts: &[Vec3]) -> Vec<(f32, f32)> {
    let n = pts.len();
    let raw: Vec<Option<(f32, f32)>> = (0..n)
        .map(|i| {
            let (a, b) = (pts[i.saturating_sub(1)], pts[(i + 1).min(n - 1)]);
            let d = Vec3::new(b.x - a.x, 0.0, b.z - a.z).normalize_or(Vec3::NEG_Z);
            paved_run(ground, pts[i], d.cross(Vec3::Y)).map(|(l, r)| (0.5 * (l + r), r - l))
        })
        .collect();
    if raw.iter().all(Option::is_none) {
        return vec![(0.0, 0.0); n];
    }
    let filled: Vec<(f32, f32)> = (0..n)
        .map(|i| (0..n).flat_map(|k| [i.checked_sub(k), Some(i + k)]).flatten().filter(|&j| j < n).find_map(|j| raw[j]).unwrap_or((0.0, 0.0)))
        .collect();
    let med = |v: &mut Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let mut out: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let (mut c, mut wd): (Vec<f32>, Vec<f32>) = (i.saturating_sub(2)..(i + 3).min(n)).map(|j| filled[j]).unzip();
            (med(&mut c).clamp(-MAX_SHIFT, MAX_SHIFT), med(&mut wd))
        })
        .collect();
    // The shift changes at most SHIFT_SLOPE m per m of road (a lay-by or junction mouth widening the paved run moved the
    // lane sideways in one step and cars couldn't follow: physics_follow max error 6.5 m). Both directions.
    let dist = |i: usize, j: usize| Vec3::new(pts[i].x - pts[j].x, 0.0, pts[i].z - pts[j].z).length();
    for i in 1..n {
        let lim = SHIFT_SLOPE * dist(i, i - 1);
        out[i].0 = out[i].0.clamp(out[i - 1].0 - lim, out[i - 1].0 + lim);
    }
    for i in (0..n.saturating_sub(1)).rev() {
        let lim = SHIFT_SLOPE * dist(i, i + 1);
        out[i].0 = out[i].0.clamp(out[i + 1].0 - lim, out[i + 1].0 + lim);
    }
    out
}

/// Largest change of the lane-fit shift per metre along the road.
const SHIFT_SLOPE: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoadType {
    Dirt,
    B,
    A,
    Freeway,
}

impl RoadType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "dirt" => Self::Dirt,
            "b" => Self::B,
            "a" => Self::A,
            "freeway" => Self::Freeway,
            _ => return None,
        })
    }

    /// Offset of a two-way road's lane centre from the way (m, OUR guess: ~3.6-4 m lanes, narrower dirt tracks).
    fn two_way_offset(self) -> f32 {
        match self {
            Self::Dirt => 1.4,
            Self::B => 1.8,
            Self::A | Self::Freeway => 2.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lane {
    /// Engine-space centre line, in travel order.
    pub pts: Vec<Vec3>,
    /// Distance along the lane at each point.
    pub cum: Vec<f32>,
    pub length: f32,
    pub road: RoadType,
    /// `traffic_density` id of the way.
    pub density: u32,
    /// Road traffic may use it (`traffic_disabled` unset). Disabled ways (the festival site, many dirt / b roads) keep
    /// their festival density: festival drivers still use them (INFERRED from festival density 8 = 1 on them).
    pub road_traffic: bool,
    /// Junction nodes (nav node index) at the start / end.
    pub from: u32,
    pub to: u32,
    /// Lanes that continue from `to` (U-turns onto the twin only at dead ends).
    pub next: Vec<u32>,
    /// The opposite-direction lane of the same road (two-way roads).
    pub twin: Option<u32>,
    /// Road index (lanes of one road share it).
    pub road_id: u32,
}

impl Lane {
    /// Position and unit tangent at distance `s` (clamped).
    pub fn at(&self, s: f32) -> (Vec3, Vec3) {
        let s = s.clamp(0.0, self.length);
        let i = match self.cum.binary_search_by(|c| c.total_cmp(&s)) {
            Ok(i) => i.min(self.pts.len() - 2),
            Err(i) => i.saturating_sub(1).min(self.pts.len() - 2),
        };
        let (a, b) = (self.pts[i], self.pts[i + 1]);
        let seg = (self.cum[i + 1] - self.cum[i]).max(1e-4);
        let t = ((s - self.cum[i]) / seg).clamp(0.0, 1.0);
        (a.lerp(b, t), (b - a).normalize_or(Vec3::NEG_Z))
    }

    /// Closest distance along the lane to `p` (ground plane), searching segments near `hint`.
    pub fn project(&self, p: Vec3, hint: f32) -> (f32, f32) {
        let mut best = (hint.clamp(0.0, self.length), f32::INFINITY);
        for i in 0..self.pts.len() - 1 {
            if (self.cum[i] - hint).abs() > 120.0 && (self.cum[i + 1] - hint).abs() > 120.0 {
                continue;
            }
            let (a, b) = (self.pts[i], self.pts[i + 1]);
            let ab = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
            let ap = Vec3::new(p.x - a.x, 0.0, p.z - a.z);
            let t = (ap.dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
            let d = (ap - ab * t).length();
            if d < best.1 {
                best = (self.cum[i] + t * (self.cum[i + 1] - self.cum[i]), d);
            }
        }
        best
    }
}

/// A lane sample for spatial queries (every [`SAMPLE_STEP`] m).
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub lane: u32,
    pub s: f32,
    pub pos: Vec3,
}

pub const SAMPLE_STEP: f32 = 10.0;
const CELL: f32 = 100.0;

#[derive(Debug, Clone, Default)]
pub struct Network {
    pub lanes: Vec<Lane>,
    /// Number of traffic roads meeting at each nav node (junction degree).
    pub degree: HashMap<u32, u32>,
    grid: HashMap<(i32, i32), Vec<Sample>>,
}

impl Network {
    pub fn build(nav: &Nav, mirror_z: bool) -> Self {
        Self::build_on(nav, mirror_z, None)
    }

    /// As [`build`](Self::build), lanes fitted to the paved road of `ground` when given (see the module doc).
    pub fn build_on(nav: &Nav, mirror_z: bool, ground: Option<&dyn Ground>) -> Self {
        let ground = ground.filter(|_| lane_fit_enabled());
        let pos = |i: u32| {
            let p = nav.nodes[i as usize].pos;
            Vec3::new(p[0], p[1], if mirror_z { -p[2] } else { p[2] })
        };
        let traffic_ways: Vec<_> = nav
            .ways
            .iter()
            .filter(|w| {
                let t = &w.tags;
                t.get("road_type").and_then(|r| RoadType::parse(r)).is_some()
                    && t.contains_key("traffic_density")
                    && t.get("ai_disabled").is_none_or(|v| v != "true")
                    && w.nodes.len() >= 2
            })
            .collect();
        let mut uses: HashMap<u32, u32> = HashMap::new();
        for w in &traffic_ways {
            let mut seen: Vec<u32> = w.nodes.clone();
            seen.sort_unstable();
            seen.dedup();
            for n in seen {
                *uses.entry(n).or_default() += 1;
            }
        }
        let mut lanes = Vec::new();
        let mut degree: HashMap<u32, u32> = HashMap::new();
        let mut road_id = 0u32;
        let mut twins: Vec<(u32, u32)> = Vec::new();
        for w in &traffic_ways {
            let road = RoadType::parse(&w.tags["road_type"]).unwrap();
            let density: u32 = w.tags["traffic_density"].parse().unwrap_or(0);
            let oneway = w.tags.get("oneway").is_some_and(|v| v == "true");
            let road_traffic = w.tags.get("traffic_disabled").is_none_or(|v| v != "true");
            // Split at junctions (nodes another traffic way also uses).
            let mut pieces: Vec<Vec<u32>> = Vec::new();
            let mut cur = vec![w.nodes[0]];
            for &n in &w.nodes[1..] {
                if cur.last() == Some(&n) {
                    continue;
                }
                cur.push(n);
                if uses.get(&n).copied().unwrap_or(0) > 1 {
                    pieces.push(std::mem::replace(&mut cur, vec![n]));
                }
            }
            if cur.len() >= 2 {
                pieces.push(cur);
            }
            for piece in pieces {
                let centre: Vec<Vec3> = piece.iter().map(|&n| pos(n)).collect();
                let (a, b) = (piece[0], *piece.last().unwrap());
                *degree.entry(a).or_default() += 1;
                *degree.entry(b).or_default() += 1;
                let mut add = |pts: Vec<Vec3>, from: u32, to: u32| -> u32 {
                    let mut cum = vec![0.0];
                    for k in 1..pts.len() {
                        let d = pts[k] - pts[k - 1];
                        cum.push(cum[k - 1] + Vec3::new(d.x, 0.0, d.z).length());
                    }
                    let length = *cum.last().unwrap();
                    lanes.push(Lane { pts, cum, length, road, density, road_traffic, from, to, next: Vec::new(), twin: None, road_id });
                    lanes.len() as u32 - 1
                };
                // Paved-road fit (dirt tracks keep the nav line: no paved run to measure).
                let fit = match ground {
                    // Freeways and one-way roads only: two-way A / B roads already sat well inside their paved run
                    // (lanes_inside_road 99%+), and their junction mouths / lay-bys only made the fit wander.
                    Some(g) if road == RoadType::Freeway || (oneway && road != RoadType::Dirt) => fit_line(g, &centre),
                    _ => vec![(0.0, 0.0); centre.len()],
                };
                let fitted = fit.iter().any(|f| f.1 > 0.0);
                let median_width = {
                    let mut w: Vec<f32> = fit.iter().map(|f| f.1).filter(|&w| w > 0.0).collect();
                    w.sort_by(f32::total_cmp);
                    w.get(w.len() / 2).copied().unwrap_or(0.0)
                };
                let shift = |o: f32| -> Vec<f32> { fit.iter().map(|f| f.0 + o).collect() };
                // Reverse travel: the centre shift flips sign (right of travel is the forward line's left).
                let rev_shift = |o: f32| -> Vec<f32> { fit.iter().rev().map(|f| -f.0 + o).collect() };
                let rev: Vec<Vec3> = centre.iter().rev().copied().collect();
                if oneway {
                    let offsets: Vec<f32> = if fitted && median_width >= 2.0 * LANE_W + 1.0 {
                        // Lanes side by side, centred in the paved run (as many as fit: freeways up to 3, others 2).
                        let max = if road == RoadType::Freeway { 3.0 } else { 2.0 };
                        let n = ((median_width - 1.0) / LANE_W).floor().clamp(2.0, max) as usize;
                        (0..n).map(|k| (k as f32 - 0.5 * (n - 1) as f32) * LANE_W).collect()
                    } else if road == RoadType::Freeway && !fitted {
                        vec![-1.9, 1.9]
                    } else {
                        vec![0.0]
                    };
                    for o in offsets {
                        add(offset_line_var(&centre, &shift(o)), a, b);
                    }
                } else {
                    // Two lanes per direction on wide two-way roads (undivided freeway, ~18 m paved).
                    let os: Vec<f32> = if fitted && median_width >= 16.0 { vec![0.5 * LANE_W, 1.5 * LANE_W] } else { vec![road.two_way_offset()] };
                    for o in os {
                        let fwd = add(offset_line_var(&centre, &shift(o)), a, b);
                        let back = add(offset_line_var(&rev, &rev_shift(o)), b, a);
                        twins.push((fwd, back));
                    }
                }
                for (fwd, back) in twins.drain(..) {
                    lanes[fwd as usize].twin = Some(back);
                    lanes[back as usize].twin = Some(fwd);
                }
                road_id += 1;
            }
        }
        // Connectivity.
        let mut by_from: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, l) in lanes.iter().enumerate() {
            by_from.entry(l.from).or_default().push(i as u32);
        }
        for i in 0..lanes.len() {
            let l = &lanes[i];
            let outs = by_from.get(&l.to).cloned().unwrap_or_default();
            let mut next: Vec<u32> = outs.iter().copied().filter(|&o| Some(o) != l.twin && lanes[o as usize].road_id != l.road_id).collect();
            if next.is_empty() {
                next = outs; // dead end: turn round
            }
            lanes[i].next = next;
        }
        let mut grid: HashMap<(i32, i32), Vec<Sample>> = HashMap::new();
        for (i, l) in lanes.iter().enumerate() {
            let n = (l.length / SAMPLE_STEP).ceil().max(1.0) as usize;
            for k in 0..n {
                let s = (k as f32 + 0.5) * l.length / n as f32;
                let (p, _) = l.at(s);
                grid.entry(cell(p)).or_default().push(Sample { lane: i as u32, s, pos: p });
            }
        }
        Self { lanes, degree, grid }
    }

    /// Lane samples within `r` of `p` (ground plane).
    pub fn samples_near(&self, p: Vec3, r: f32) -> impl Iterator<Item = &Sample> + '_ {
        let (cx, cz) = cell(p);
        let n = (r / CELL).ceil() as i32;
        let r2 = r * r;
        (-n..=n).flat_map(move |dx| (-n..=n).map(move |dz| (cx + dx, cz + dz))).filter_map(move |c| self.grid.get(&c)).flatten().filter(move |s| {
            let d = s.pos - p;
            d.x * d.x + d.z * d.z <= r2
        })
    }

    /// The lane point nearest to `p` within `r` whose direction agrees with `heading` (if given): (lane, s, distance).
    pub fn nearest(&self, p: Vec3, r: f32, heading: Option<Vec3>) -> Option<(u32, f32, f32)> {
        let mut best: Option<(u32, f32, f32)> = None;
        for smp in self.samples_near(p, r + SAMPLE_STEP) {
            let lane = &self.lanes[smp.lane as usize];
            let (s, d) = lane.project(p, smp.s);
            if d > r || best.is_some_and(|b| b.2 <= d) {
                continue;
            }
            if let Some(h) = heading {
                if lane.at(s).1.dot(h) < 0.3 {
                    continue;
                }
            }
            best = Some((smp.lane, s, d));
        }
        best
    }

    /// Is the node a junction (three or more road ends meet)?
    pub fn is_junction(&self, node: u32) -> bool {
        self.degree.get(&node).copied().unwrap_or(0) >= 3
    }
}

fn cell(p: Vec3) -> (i32, i32) {
    ((p.x / CELL).floor() as i32, (p.z / CELL).floor() as i32)
}

/// The polyline offset `o[i]` m to the right of travel at each point (mitred at the inner points).
fn offset_line_var(pts: &[Vec3], o: &[f32]) -> Vec<Vec3> {
    if o.iter().all(|&x| x == 0.0) {
        return pts.to_vec();
    }
    let n = pts.len();
    (0..n)
        .map(|i| {
            let a = pts[i.saturating_sub(1)];
            let b = pts[(i + 1).min(n - 1)];
            let d = Vec3::new(b.x - a.x, 0.0, b.z - a.z).normalize_or(Vec3::NEG_Z);
            // Right of travel in engine space (Y up, -Z forward): forward × up.
            let right = d.cross(Vec3::Y);
            pts[i] + right * o[i]
        })
        .collect()
}
