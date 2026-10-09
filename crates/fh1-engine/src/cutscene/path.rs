//! Cutscene data (serde mirror of `<assets>/story/cutscenes/<name>.json`) and the pure camera-path maths.
//!
//! No Bevy types here: everything is plain `f32` so the tests run without an app.
//!
//! Key channels (all RAW file values except `pos`, which the setup already put into engine space, Z negated):
//! `yaw`, `pitch`, `roll`, `fov` in degrees. See `cutscene.rs` for the angle conventions that were fitted on the data.

use serde::{Deserialize, Deserializer};

/// `null` or missing -> the type's default (the setup JSON uses null freely).
fn nz<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// bool, 0/1 number, "0"/"1"/"true" string or null.
fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::Bool(b)) => b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(serde_json::Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        _ => false,
    })
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Key {
    pub t: f32,
    /// Engine space (Z already negated); relative to the cam's frame for CarSpace / PartSpace.
    pub pos: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    /// Degrees (vertical, INFERRED).
    pub fov: f32,
}

impl Default for Key {
    fn default() -> Self {
        Self { t: 0.0, pos: [0.0; 3], yaw: 0.0, pitch: 0.0, roll: 0.0, fov: 45.0 }
    }
}

impl Key {
    /// x y z yaw pitch roll fov.
    fn chan(&self) -> [f32; 7] {
        [self.pos[0], self.pos[1], self.pos[2], self.yaw, self.pitch, self.roll, self.fov]
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Fade {
    #[serde(deserialize_with = "nz")]
    pub duration: f32,
    #[serde(deserialize_with = "nz")]
    pub hold: f32,
    #[serde(deserialize_with = "nz")]
    pub curve: String,
    #[serde(deserialize_with = "nz")]
    pub color: [f32; 3],
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Cam {
    #[serde(rename = "type", deserialize_with = "nz")]
    pub kind: String,
    #[serde(deserialize_with = "nz")]
    pub start_cut: f32,
    #[serde(deserialize_with = "nz")]
    pub duration: f32,
    #[serde(deserialize_with = "nz")]
    pub target_name: String,
    #[serde(deserialize_with = "nz")]
    pub pos_space: String,
    #[serde(deserialize_with = "nz")]
    pub rot_space: String,
    #[serde(deserialize_with = "nz")]
    pub time_anim: String,
    #[serde(deserialize_with = "flag")]
    pub is_in_cockpit: bool,
    #[serde(deserialize_with = "nz")]
    pub ease_in: f32,
    #[serde(deserialize_with = "nz")]
    pub ease_out: f32,
    #[serde(deserialize_with = "flag")]
    pub disable_car_rendering: bool,
    pub fade_in: Option<Fade>,
    pub fade_out: Option<Fade>,
    #[serde(deserialize_with = "nz")]
    pub keys: Vec<Key>,
}

impl Cam {
    pub fn world_space(&self) -> bool {
        self.pos_space.eq_ignore_ascii_case("WorldSpace")
    }

    /// Local time of this cam at cutscene time `t`.
    pub fn local(&self, t: f32) -> f32 {
        t - self.start_cut
    }

    /// End time in cutscene seconds: `start_cut + duration` (or the last key if the duration is missing).
    pub fn end(&self) -> f32 {
        let d = if self.duration > 0.0 { self.duration } else { self.keys.last().map_or(0.0, |k| k.t) };
        self.start_cut + d
    }

    /// Sort keys by time (stable: equal-t keys keep file order) and unwrap the angles so they take the short way round.
    pub fn prepare(&mut self) {
        self.keys.retain(|k| k.t.is_finite() && k.pos.iter().all(|v| v.is_finite()));
        self.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        for i in 1..self.keys.len() {
            let (p, k) = (self.keys[i - 1].clone(), &mut self.keys[i]);
            k.yaw = p.yaw + wrap180(k.yaw - p.yaw);
            k.pitch = p.pitch + wrap180(k.pitch - p.pitch);
            k.roll = p.roll + wrap180(k.roll - p.roll);
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Trigger {
    #[serde(deserialize_with = "nz")]
    pub class: String,
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(deserialize_with = "nz")]
    pub parent: String,
    pub engine: Option<[f32; 3]>,
    #[serde(deserialize_with = "nz")]
    pub attrs: serde_json::Map<String, serde_json::Value>,
}

impl Trigger {
    /// A string attribute (`event`, `group`, ...).
    pub fn attr_str(&self, k: &str) -> String {
        match self.attrs.get(k) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    pub fn attr_f32(&self, k: &str) -> Option<f32> {
        self.attrs.get(k).and_then(|v| v.as_f64()).map(|v| v as f32)
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Event {
    #[serde(deserialize_with = "nz")]
    pub time: f32,
    #[serde(deserialize_with = "nz")]
    pub triggers: Vec<Trigger>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Cutscene {
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(deserialize_with = "nz")]
    pub group: String,
    #[serde(deserialize_with = "nz")]
    pub file: String,
    pub playback_mode: Option<String>,
    #[serde(deserialize_with = "nz")]
    pub duration_s: f32,
    #[serde(deserialize_with = "nz")]
    pub loop_from_s: f32,
    #[serde(deserialize_with = "flag")]
    pub mirror_rhd: bool,
    #[serde(deserialize_with = "nz")]
    pub cams: Vec<Cam>,
    #[serde(deserialize_with = "nz")]
    pub events: Vec<Event>,
}

impl Cutscene {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let mut c: Cutscene = serde_json::from_str(json)?;
        for cam in &mut c.cams {
            cam.prepare();
        }
        // Events come unsorted in the file (Opening_Cutscene lists 6.5 s before 5.8 s).
        c.events.sort_by(|a, b| a.time.total_cmp(&b.time));
        Ok(c)
    }

    pub fn looping(&self) -> bool {
        self.playback_mode.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("Looping"))
    }

    /// Total length (s): `duration_s`, or the latest cam end when that is missing.
    pub fn length(&self) -> f32 {
        if self.duration_s > 0.0 {
            self.duration_s
        } else {
            self.cams.iter().map(Cam::end).fold(0.0, f32::max)
        }
    }

    /// The cam that owns the picture at `t`: the last one that has started and not ended; between cams (a gap) or after
    /// the last end the previous cam keeps holding its final key. None before the first cam starts (hold cam 0).
    pub fn active_cam(&self, t: f32) -> Option<usize> {
        if self.cams.is_empty() {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, c) in self.cams.iter().enumerate() {
            if c.start_cut <= t && best.is_none_or(|b| c.start_cut >= self.cams[b].start_cut) {
                best = Some(i);
            }
        }
        Some(best.unwrap_or_else(|| {
            // Before every start: the earliest cam.
            self.cams.iter().enumerate().min_by(|a, b| a.1.start_cut.total_cmp(&b.1.start_cut)).map_or(0, |(i, _)| i)
        }))
    }

    /// Fade overlay at `t`: (colour rgb 0..1, alpha 0..1). Over all cams the strongest contribution wins.
    ///
    /// INFERRED semantics (Opening_Cutscene: FadeIn Duration 2.0, Hold 0.5, "S"): fade-in = fully covered for `hold` seconds
    /// from the cam start, then the cover fades out over `duration`; fade-out = the cover rises over the last `duration`
    /// seconds of the cam.
    pub fn fade_at(&self, t: f32) -> Option<([f32; 3], f32)> {
        let mut best: Option<([f32; 3], f32)> = None;
        for c in &self.cams {
            let local = c.local(t);
            let len = c.end() - c.start_cut;
            if local < 0.0 || local > len {
                continue;
            }
            let mut put = |f: &Fade, a: f32| {
                if a > 0.0 && best.is_none_or(|b| a > b.1) {
                    best = Some((fade_color(f), a.clamp(0.0, 1.0)));
                }
            };
            if let Some(f) = &c.fade_in {
                let a = if local < f.hold {
                    1.0
                } else if f.duration > 1e-4 && local < f.hold + f.duration {
                    1.0 - curve(&f.curve, (local - f.hold) / f.duration)
                } else {
                    0.0
                };
                put(f, a);
            }
            if let Some(f) = &c.fade_out {
                if f.duration > 1e-4 && local > len - f.duration {
                    put(f, curve(&f.curve, (local - (len - f.duration)) / f.duration));
                }
            }
        }
        best
    }
}

fn fade_color(f: &Fade) -> [f32; 3] {
    let c = f.color;
    // 0..1 in the data; tolerate 0..255.
    let k = if c.iter().any(|v| *v > 1.0) { 1.0 / 255.0 } else { 1.0 };
    [c[0] * k, c[1] * k, c[2] * k]
}

fn curve(kind: &str, u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    if kind.eq_ignore_ascii_case("S") {
        u * u * (3.0 - 2.0 * u)
    } else {
        u
    }
}

/// Wrap an angle difference (degrees) into [-180, 180).
pub fn wrap180(a: f32) -> f32 {
    (a + 180.0).rem_euclid(360.0) - 180.0
}

/// One evaluated camera pose (file angles, degrees; angles may exceed +-180 after unwrapping).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub pos: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub fov: f32,
}

fn sample_of(c: [f32; 7]) -> Sample {
    Sample { pos: [c[0], c[1], c[2]], yaw: c[3], pitch: c[4], roll: c[5], fov: c[6] }
}

/// Finite-difference tangent (per second) at key `i`; zero where the time step is degenerate.
fn tangent(keys: &[Key], i: usize) -> [f32; 7] {
    let (a, b) = if i == 0 {
        (0, 1usize.min(keys.len() - 1))
    } else if i + 1 == keys.len() {
        (i - 1, i)
    } else {
        (i - 1, i + 1)
    };
    let dt = keys[b].t - keys[a].t;
    if dt <= 1e-6 {
        return [0.0; 7];
    }
    let (ca, cb) = (keys[a].chan(), keys[b].chan());
    let mut m = [0.0; 7];
    for j in 0..7 {
        m[j] = (cb[j] - ca[j]) / dt;
    }
    m
}

/// Evaluate the track at local time `t` (clamped to the first/last key): cubic Hermite with Catmull-Rom tangents on the
/// (non-uniform) key times, for position and the unwrapped angles alike. Exact at key times. Keys with equal `t` never divide
/// by zero: the later one wins from that time on. `keys` must be `Cam::prepare`d (sorted, angles unwrapped).
pub fn eval(keys: &[Key], t: f32) -> Option<Sample> {
    if keys.is_empty() {
        return None;
    }
    let t = if t.is_finite() { t } else { 0.0 };
    let n = keys.partition_point(|k| k.t <= t);
    if n == 0 {
        return Some(sample_of(keys[0].chan()));
    }
    if n == keys.len() {
        return Some(sample_of(keys[n - 1].chan()));
    }
    let (i0, i1) = (n - 1, n);
    let (k0, k1) = (&keys[i0], &keys[i1]);
    let dt = k1.t - k0.t;
    if dt <= 1e-6 {
        return Some(sample_of(k0.chan()));
    }
    let u = ((t - k0.t) / dt).clamp(0.0, 1.0);
    let (p0, p1) = (k0.chan(), k1.chan());
    let (m0, m1) = (tangent(keys, i0), tangent(keys, i1));
    let (u2, u3) = (u * u, u * u * u);
    let (h00, h10, h01, h11) = (2.0 * u3 - 3.0 * u2 + 1.0, u3 - 2.0 * u2 + u, -2.0 * u3 + 3.0 * u2, u3 - u2);
    let mut out = [0.0; 7];
    for j in 0..7 {
        out[j] = h00 * p0[j] + h10 * dt * m0[j] + h01 * p1[j] + h11 * dt * m1[j];
    }
    Some(sample_of(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(t: f32, x: f32, yaw: f32) -> Key {
        Key { t, pos: [x, x * 0.5, -x], yaw, pitch: yaw * 0.1, roll: 0.0, fov: 40.0 + x }
    }

    fn cam(keys: Vec<Key>) -> Cam {
        let mut c = Cam { keys, ..Default::default() };
        c.prepare();
        c
    }

    fn same_angle(a: f32, b: f32) -> bool {
        wrap180(a - b).abs() < 1e-3
    }

    #[test]
    fn exact_at_key_times() {
        let c = cam(vec![key(0.0, 0.0, 10.0), key(0.7, 2.0, 30.0), key(1.5, 5.0, 20.0), key(3.0, 4.0, 90.0)]);
        for k in &c.keys {
            let s = eval(&c.keys, k.t).unwrap();
            assert_eq!(s.pos, k.pos, "pos at t={}", k.t);
            assert!(same_angle(s.yaw, k.yaw), "yaw at t={}", k.t);
            assert!((s.fov - k.fov).abs() < 1e-5);
        }
        // Clamped outside the range.
        assert_eq!(eval(&c.keys, -5.0).unwrap().pos, c.keys[0].pos);
        assert_eq!(eval(&c.keys, 99.0).unwrap().pos, c.keys[3].pos);
        assert!(eval(&[], 0.0).is_none());
    }

    #[test]
    fn equal_times_do_not_nan() {
        let c = cam(vec![key(0.0, 0.0, 0.0), key(1.0, 1.0, 5.0), key(1.0, 3.0, 9.0), key(1.0, 4.0, 9.0), key(2.0, 6.0, 12.0)]);
        for i in 0..=400 {
            let t = i as f32 * 0.005;
            let s = eval(&c.keys, t).unwrap();
            assert!(s.pos.iter().all(|v| v.is_finite()) && s.yaw.is_finite() && s.pitch.is_finite() && s.fov.is_finite(), "t={t} {s:?}");
        }
        // A single key and an all-equal track.
        assert!(eval(&cam(vec![key(1.0, 1.0, 1.0)]).keys, 5.0).unwrap().pos[0].is_finite());
        let z = cam(vec![key(1.0, 1.0, 1.0), key(1.0, 2.0, 2.0)]);
        assert!(eval(&z.keys, 1.0).unwrap().pos[0].is_finite());
    }

    #[test]
    fn angle_takes_the_short_way() {
        let c = cam(vec![key(0.0, 0.0, 350.0), key(1.0, 0.0, 10.0)]);
        let mid = eval(&c.keys, 0.5).unwrap();
        // 350 -> 10 passes 0 (or 360), never 180.
        assert!(wrap180(mid.yaw).abs() < 1.0, "mid yaw {}", mid.yaw);
        assert!(same_angle(eval(&c.keys, 0.0).unwrap().yaw, 350.0));
        assert!(same_angle(eval(&c.keys, 1.0).unwrap().yaw, 10.0));
        assert!((wrap180(10.0 - 350.0) - 20.0).abs() < 1e-4);
    }

    #[test]
    fn parses_tiny_json_with_missing_fields() {
        let c = Cutscene::parse(r#"{"name":"x","cams":[{"keys":[{"t":1.0},{"t":0.0,"pos":[1,2,3],"yaw":5}],"fade_in":{"duration":2}}],"events":[{"time":2},{"time":1,"triggers":[{"class":"A","attrs":{"event":"E"}}]}]}"#).unwrap();
        assert_eq!(c.name, "x");
        assert!(c.playback_mode.is_none() && !c.looping());
        assert_eq!(c.cams.len(), 1);
        assert_eq!(c.cams[0].keys[0].t, 0.0, "keys sorted");
        assert_eq!(c.cams[0].keys[0].pos, [1.0, 2.0, 3.0]);
        assert_eq!(c.cams[0].keys[1].fov, 45.0);
        assert_eq!(c.events[0].time, 1.0, "events sorted");
        assert_eq!(c.events[0].triggers[0].attr_str("event"), "E");
        assert!(Cutscene::parse("{}").is_ok());
        assert!(Cutscene::parse(r#"{"cams":[{"type":null,"is_in_cockpit":1,"keys":null}],"playback_mode":"Looping"}"#).unwrap().looping());
    }

    #[test]
    fn fades_and_active_cam() {
        let c = Cutscene::parse(
            r#"{"duration_s":10,"cams":[
              {"start_cut":0,"duration":4,"fade_in":{"duration":2,"hold":0.5,"curve":"S","color":[0,0,0]},"keys":[{"t":0},{"t":4}]},
              {"start_cut":4,"duration":6,"fade_out":{"duration":1},"keys":[{"t":0},{"t":6}]}]}"#,
        )
        .unwrap();
        assert_eq!(c.active_cam(0.0), Some(0));
        assert_eq!(c.active_cam(3.99), Some(0));
        assert_eq!(c.active_cam(4.0), Some(1));
        assert_eq!(c.active_cam(50.0), Some(1));
        assert_eq!(c.length(), 10.0);
        assert!((c.fade_at(0.25).unwrap().1 - 1.0).abs() < 1e-5);
        assert!(c.fade_at(1.5).unwrap().1 < 1.0);
        assert!(c.fade_at(3.0).is_none());
        assert!(c.fade_at(9.99).unwrap().1 > 0.9);
    }

    /// Disc/data-gated: every track of the full story dump evaluates to finite numbers at 50 points.
    #[test]
    fn story_camera_tracks_are_finite() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/extracted/story/camera_tracks.json");
        let Ok(text) = std::fs::read_to_string(&p) else {
            eprintln!("skip: {} absent", p.display());
            return;
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let tracks = v["tracks"].as_array().unwrap();
        assert!(!tracks.is_empty());
        let mut n = 0;
        for t in tracks {
            let Some(keys) = t["cam"]["keys"].as_array() else { continue };
            let mut c = Cam { keys: keys.iter().map(|k| serde_json::from_value::<Key>(k.clone()).unwrap()).collect(), ..Default::default() };
            c.prepare();
            let Some(last) = c.keys.last().map(|k| k.t) else { continue };
            for i in 0..50 {
                let tt = last * i as f32 / 49.0;
                let s = eval(&c.keys, tt).unwrap();
                assert!(s.pos.iter().all(|v| v.is_finite()) && s.yaw.is_finite() && s.pitch.is_finite() && s.roll.is_finite() && s.fov.is_finite(), "{:?} t={tt}", t["cutscene"]);
            }
            n += 1;
        }
        eprintln!("{n} tracks evaluated");
    }
}
