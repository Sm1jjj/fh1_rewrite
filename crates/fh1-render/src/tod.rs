//! Time-of-day curves (`media/tracks/<track>/Ribbon_00/TimeOfDay{A,B,Neutral}.xml`).
//!
//! `<TOD version="2">` holds one element per channel (`<SunColor numKeys="26">`) with
//! `<Key time="t" x=".." [y=".." z=".."]>` children. Time is in minutes, 0..1440. Values are
//! interpolated linearly between keys (how the game interpolates and wraps is UNVERIFIED;
//! being checked against default.xex, Function_825D1CB8 loads the file).

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Curve {
    /// (time in minutes, value) sorted by time.
    pub keys: Vec<(f32, [f32; 3])>,
}

impl Curve {
    pub fn eval(&self, minutes: f32) -> [f32; 3] {
        let k = &self.keys;
        match k.len() {
            0 => [0.0; 3],
            1 => k[0].1,
            _ => {
                let t = minutes.rem_euclid(1440.0);
                if t <= k[0].0 {
                    return k[0].1;
                }
                if t >= k[k.len() - 1].0 {
                    return k[k.len() - 1].1;
                }
                let i = k.partition_point(|(kt, _)| *kt <= t);
                let (t0, a) = k[i - 1];
                let (t1, b) = k[i];
                let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
                [0, 1, 2].map(|j| a[j] + (b[j] - a[j]) * f)
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TimeOfDay {
    pub channels: HashMap<String, Curve>,
}

fn attr(tag: &str, name: &str) -> Option<f32> {
    let pat = format!(" {name}=\"");
    let s = tag.find(&pat)? + pat.len();
    let e = tag[s..].find('"')? + s;
    tag[s..e].trim().parse().ok()
}

impl TimeOfDay {
    pub fn parse(xml: &str) -> Self {
        let mut channels = HashMap::new();
        let mut current: Option<(String, Curve)> = None;
        for raw in xml.split('<').skip(1) {
            let tag = raw.split('>').next().unwrap_or("");
            if let Some(rest) = tag.strip_prefix("Key ") {
                if let Some((_, c)) = current.as_mut() {
                    let t = attr(&format!(" {rest}"), "time").unwrap_or(0.0);
                    let v = [" x", " y", " z"].map(|n| attr(&format!(" {rest}"), n.trim()).unwrap_or(0.0));
                    c.keys.push((t, v));
                }
            } else if let Some(name) = tag.strip_prefix('/') {
                if let Some((n, c)) = current.take() {
                    if n == name.trim() {
                        channels.insert(n, c);
                    } else {
                        current = Some((n, c));
                    }
                }
            } else if tag.contains("numKeys=") {
                let name = tag.split_whitespace().next().unwrap_or("").to_string();
                current = Some((name, Curve::default()));
            }
        }
        for c in channels.values_mut() {
            c.keys.sort_by(|a: &(f32, [f32; 3]), b| a.0.total_cmp(&b.0));
        }
        Self { channels }
    }

    pub fn get(&self, name: &str, minutes: f32) -> [f32; 3] {
        self.channels.get(name).map(|c| c.eval(minutes)).unwrap_or([0.0; 3])
    }

    pub fn scalar(&self, name: &str, minutes: f32) -> f32 {
        self.get(name, minutes)[0]
    }

    /// A channel the file may leave out: the evaluator then keeps the parameter block's
    /// constructor value (0x825C7C98), passed here as `default`.
    pub fn get_or(&self, name: &str, minutes: f32, default: [f32; 3]) -> [f32; 3] {
        self.channels.get(name).map(|c| c.eval(minutes)).unwrap_or(default)
    }

    pub fn scalar_or(&self, name: &str, minutes: f32, default: f32) -> f32 {
        self.get_or(name, minutes, [default; 3])[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_interpolates() {
        let x = r#"<TOD version="2"> <SunColorMult numKeys="2">
  <Key time="0.000000" x="1.0"></Key>
  <Key time="100.000000" x="3.0"></Key>
 </SunColorMult>
 <SunPos numKeys="1"><Key time="5" x="1" y="2" z="3"></Key></SunPos></TOD>"#;
        let t = TimeOfDay::parse(x);
        assert_eq!(t.scalar("SunColorMult", 50.0), 2.0);
        assert_eq!(t.get("SunPos", 700.0), [1.0, 2.0, 3.0]);
    }
}
