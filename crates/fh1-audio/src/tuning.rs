//! Per-car audio tuning from `media/audio/cars/{CarModelTuning,Engines/EngineTuning,
//! Engines/HarmonicTuning}.zip`, resolved into one [`CarAudio`] per car.
//!
//! Chain (verified on the EU disc, 172/177 cars complete; the rest fall back to defaults):
//! `<Car>_ET.xml` (engine tuning) names one `*_HT.xml` (harmonic tuning) per emitter and
//! upgrade level (`L0` = stock), and each HT names its `.fsb` bank in
//! `Engines/Soundbanks/LOD1/` plus the RPM loops in it. `<Car>_CMT.xml` (car model tuning)
//! has per-camera emitter volumes. `NA` means "no such emitter".
//!
//! Every tunable is a [`Curve`]: a three-point piecewise-linear curve over a weighted sum of
//! normalised RPM, throttle, positive and negative torque.
//!
//! ET `EmissionGroup0/1/2` = intake / engine ambient / exhaust: verified, because each
//! group's Volume curve equals the `<DSP>` Volume of the HT it is paired with (e.g. ALF_8C_08).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- tiny DOM

#[derive(Debug, Default)]
pub struct El {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub kids: Vec<El>,
}

impl El {
    pub fn parse(xml: &str) -> Result<El> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut stack = vec![El::default()];
        loop {
            match reader.read_event()? {
                Event::Start(e) => stack.push(open(&e)?),
                Event::Empty(e) => {
                    let el = open(&e)?;
                    stack.last_mut().unwrap().kids.push(el);
                }
                Event::End(_) => {
                    let el = stack.pop().unwrap();
                    stack.last_mut().context("unbalanced XML")?.kids.push(el);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        let mut root = stack.pop().unwrap();
        root.kids.pop().context("empty XML")
    }
    pub fn child(&self, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.name.eq_ignore_ascii_case(name))
    }
    pub fn path(&self, path: &[&str]) -> Option<&El> {
        path.iter().try_fold(self, |el, n| el.child(n))
    }
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
    pub fn f(&self, key: &str) -> Option<f32> {
        self.attr(key)?.trim().parse().ok()
    }
}

fn open(e: &BytesStart) -> Result<El> {
    let mut el = El { name: AsRef::<str>::as_ref(&e.name()).to_owned(), ..Default::default() };
    for a in e.attributes() {
        let a = a?;
        el.attrs.push((
            AsRef::<str>::as_ref(&a.key).to_owned(),
            a.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
        ));
    }
    Ok(el)
}

// ---------------------------------------------------------------- curves

/// Engine state every curve is driven by, all normalised to 0..1.
#[derive(Debug, Clone, Copy, Default)]
pub struct Drive {
    /// (rpm - idle) / (redline - idle).
    pub rpm: f32,
    pub throttle: f32,
    /// Engine torque / peak torque when driving.
    pub pos_torque: f32,
    /// Engine braking torque / peak torque (overrun).
    pub neg_torque: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Curve {
    /// Weights of rpm, throttle, pos_torque, neg_torque.
    pub coeff: [f32; 4],
    pub pts: [[f32; 2]; 3],
}

impl Curve {
    pub const fn constant(v: f32) -> Curve {
        Curve { coeff: [0.0; 4], pts: [[0.0, v], [0.5, v], [1.0, v]] }
    }

    fn parse(el: &El) -> Option<Curve> {
        let c = el.child("PhysicsCoeff")?;
        let p = el.child("ThreePointCurve")?;
        let g = |e: &El, k| e.f(k).unwrap_or(0.0);
        Some(Curve {
            coeff: [g(c, "RPM"), g(c, "Throttle"), g(c, "PosTorque"), g(c, "NegTorque")],
            pts: [[g(p, "x0"), g(p, "y0")], [g(p, "x1"), g(p, "y1")], [g(p, "x2"), g(p, "y2")]],
        })
    }

    /// UNVERIFIED: input = clamp(Σ coeff·input, 0, 1); points with x not increasing (the
    /// disc has e.g. x = 0, 0, 0 on curves whose coefficients are all zero) are skipped.
    pub fn eval(&self, d: &Drive) -> f32 {
        let c = self.coeff;
        let x = (c[0] * d.rpm + c[1] * d.throttle + c[2] * d.pos_torque + c[3] * d.neg_torque).clamp(0.0, 1.0);
        let [p0, p1, p2] = self.pts;
        let seg = |a: [f32; 2], b: [f32; 2]| {
            if b[0] > a[0] {
                a[1] + (b[1] - a[1]) * ((x - a[0]) / (b[0] - a[0])).clamp(0.0, 1.0)
            } else {
                a[1]
            }
        };
        if x <= p0[0] {
            p0[1]
        } else if x <= p1[0] || p2[0] <= p1[0] {
            seg(p0, p1)
        } else {
            seg(p1, p2)
        }
    }
}

// ---------------------------------------------------------------- resolved model

/// One RPM loop of a harmonic bank.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    /// Index into the bank's samples.
    pub sample: u32,
    pub rpm_min: f32,
    pub rpm_sample: f32,
    pub rpm_max: f32,
    pub volume: f32,
    pub pitch: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peq {
    /// Linear gain multiplier at the centre frequency.
    pub gain: Curve,
    pub freq: Curve,
    /// Octaves (UNVERIFIED).
    pub bandwidth: Curve,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dsp {
    pub volume: Option<Curve>,
    pub peq: Option<Peq>,
    pub pos_load_peq: Option<Peq>,
    pub neg_load_peq: Option<Peq>,
    pub lowpass: Option<Curve>,
}

impl Dsp {
    fn parse(el: &El) -> Dsp {
        let peq = |e: Option<&El>| {
            let e = e.filter(|e| e.attr("Active") != Some("0"))?;
            Some(Peq {
                gain: Curve::parse(e.child("Gain")?)?,
                freq: Curve::parse(e.child("CenterFrequency")?)?,
                bandwidth: Curve::parse(e.child("Bandwidth")?)?,
            })
        };
        Dsp {
            volume: el.path(&["Volume", "Gain"]).and_then(Curve::parse),
            peq: peq(el.child("PEQ")),
            pos_load_peq: peq(el.path(&["LoadPEQ", "PosLoad"])),
            neg_load_peq: peq(el.path(&["LoadPEQ", "NegLoad"])),
            lowpass: el
                .child("Lowpass")
                .filter(|e| e.attr("Active") != Some("0"))
                .and_then(|e| Curve::parse(e.child("CutoffFrequency")?)),
        }
    }
}

/// A harmonic emitter (intake, engine ambient or exhaust): one bank, one or more loop sets
/// (the exhaust has `ExhaustL` and `ExhaustR`), and its DSP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Emitter {
    /// Bank file name, e.g. `8N2_MasGranTurismo_Exh.fsb`.
    pub bank: String,
    /// Harmonic tuning this came from (for debugging).
    pub tuning: String,
    /// Loop sets by element name (`ExhaustL`, `ExhaustR`, `EngineAmbient`, ...).
    pub sets: BTreeMap<String, Vec<Loop>>,
    pub dsp: Dsp,
    /// HT `audio_rpm_idle_gain_reset` (meaning unknown).
    pub idle_gain_reset: Option<f32>,
}

impl Emitter {
    fn parse(tuning: &str, ht: &El) -> Option<Emitter> {
        let bank = ht.attr("name")?.to_owned();
        if bank.eq_ignore_ascii_case("NA.fsb") {
            return None;
        }
        let mut sets = BTreeMap::new();
        for k in &ht.kids {
            let loops: Vec<Loop> = k
                .kids
                .iter()
                .filter(|l| l.name == "Loop")
                .filter_map(|l| {
                    Some(Loop {
                        sample: l.f("wavebankindex")? as u32,
                        rpm_min: l.f("rpm_min")?,
                        rpm_sample: l.f("rpm_sample")?,
                        rpm_max: l.f("rpm_max")?,
                        volume: l.f("volume").unwrap_or(1.0),
                        pitch: l.f("pitch").unwrap_or(1.0),
                    })
                })
                .collect();
            if !loops.is_empty() {
                sets.insert(k.name.clone(), loops);
            }
        }
        Some(Emitter {
            bank,
            tuning: tuning.to_owned(),
            sets,
            dsp: ht.child("DSP").map(Dsp::parse).unwrap_or_default(),
            idle_gain_reset: ht.child("EngineSettings").and_then(|e| e.f("audio_rpm_idle_gain_reset")),
        })
    }
}

/// Per-camera mix from the CMT `<Listener><Basic>` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewMix {
    pub intake: f32,
    pub ambient: f32,
    pub exhaust_l: f32,
    pub exhaust_r: f32,
    pub tire: f32,
    pub transmission: f32,
    pub turbo: f32,
    pub supercharger: f32,
    pub wind: f32,
}

impl Default for ViewMix {
    fn default() -> Self {
        ViewMix {
            intake: 0.7,
            ambient: 0.7,
            exhaust_l: 1.0,
            exhaust_r: 1.0,
            tire: 0.6,
            transmission: 0.6,
            turbo: 0.55,
            supercharger: 0.5,
            wind: 0.69,
        }
    }
}

/// One band of the ET `<TrashDSP>` multiband overdrive (the xex registers it as an FMOD DSP plugin with
/// "Cutoff 1/2", "Band N Effect", "BN Input Gain/Overdrive/Mix/Output Gain", 831D8440). Values as on disc:
/// gains in dB, overdrive in dB of drive (INFERRED), mix in percent wet. `UseCurves="0"` = the static attributes
/// (stored here as constant curves).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashBand {
    /// `effecttype` 0 = off, 1, 2 (soft / hard clip; INFERRED).
    pub effect: u8,
    pub input_gain: Curve,
    pub overdrive: Curve,
    pub mix: Curve,
    pub output_gain: Curve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trash {
    /// Band split frequencies, Hz.
    pub cutoff1: f32,
    pub cutoff2: f32,
    pub volume_compensate: f32,
    pub bands: [TrashBand; 3],
}

/// ET `<Distortion>`: FMOD-style distortion `Level` 0..1 and `VolumeCompensate` (read as output cut, INFERRED).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Distortion {
    pub level: Curve,
    pub volume_compensate: f32,
}

/// ET `<ShiftVolumeScalar>`: engine volume x `pct` right after a shift, back to 1 over `time` s (INFERRED).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ShiftBoost {
    pub up_pct: f32,
    pub up_time: f32,
    pub down_pct: f32,
    pub down_time: f32,
}

impl Default for ShiftBoost {
    fn default() -> Self {
        ShiftBoost { up_pct: 1.4, up_time: 0.7, down_pct: 1.4, down_time: 0.7 }
    }
}

/// Car-wide engine DSP and extras from the ET that sit after the three emitters (audio-2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineExtras {
    pub focus_peq: Option<Peq>,
    pub distortion: Option<Distortion>,
    pub trash: Option<Trash>,
    pub shift: ShiftBoost,
    pub burble_volume: f32,
    pub backfire_volume: f32,
    /// `blowoffL1Choice` (1..5): which `BlowOff_L1_<n>_*` sample.
    pub blowoff_choice: u32,
    /// ET `<ExhaustReflection LPCutoff>`.
    pub reflection_lp: Option<f32>,
}

impl EngineExtras {
    fn parse(et: &El) -> EngineExtras {
        let active = |e: &&El| e.attr("Active") != Some("0");
        let peq = |e: &El| {
            Some(Peq {
                gain: Curve::parse(e.child("Gain")?)?,
                freq: Curve::parse(e.child("CenterFrequency")?)?,
                bandwidth: Curve::parse(e.child("Bandwidth")?)?,
            })
        };
        let trash = et.child("TrashDSP").filter(active).map(|t| {
            let curves = t.attr("UseCurves") == Some("1");
            let band = |name: &str| {
                let b = t.child(name);
                let p = |k: &str, def: f32| {
                    let fixed = Curve::constant(b.and_then(|b| b.f(k)).unwrap_or(def));
                    if curves { b.and_then(|b| b.child(k)).and_then(Curve::parse).unwrap_or(fixed) } else { fixed }
                };
                TrashBand {
                    effect: b.and_then(|b| b.f("effecttype")).unwrap_or(0.0) as u8,
                    input_gain: p("inputgain", 0.0),
                    overdrive: p("overdrive", 0.0),
                    mix: p("mix", 0.0),
                    output_gain: p("outputgain", 0.0),
                }
            };
            Trash {
                cutoff1: t.f("Cutoff1").unwrap_or(400.0),
                cutoff2: t.f("Cutoff2").unwrap_or(2500.0),
                volume_compensate: t.f("VolumeCompensate").unwrap_or(0.0),
                bands: [band("Band1"), band("Band2"), band("Band3")],
            }
        });
        let vols = et.child("VolumeAdjustments");
        let s = et.child("ShiftVolumeScalar");
        let d = ShiftBoost::default();
        EngineExtras {
            focus_peq: et.child("FocusPEQ").filter(active).and_then(peq),
            distortion: et.child("Distortion").filter(active).and_then(|e| {
                Some(Distortion { level: Curve::parse(e.child("Level")?)?, volume_compensate: e.f("VolumeCompensate").unwrap_or(0.0) })
            }),
            trash,
            shift: ShiftBoost {
                up_pct: s.and_then(|s| s.f("ShiftVolBoostUpPct")).unwrap_or(d.up_pct),
                up_time: s.and_then(|s| s.f("ShiftVolBoostUpTime")).unwrap_or(d.up_time),
                down_pct: s.and_then(|s| s.f("ShiftVolBoostDownPct")).unwrap_or(d.down_pct),
                down_time: s.and_then(|s| s.f("ShiftVolBoostDownTime")).unwrap_or(d.down_time),
            },
            burble_volume: vols.and_then(|v| v.f("BurbleVolume")).unwrap_or(0.0),
            backfire_volume: vols.and_then(|v| v.f("BackfireVolume")).unwrap_or(0.0),
            blowoff_choice: et.child("EngineSettings").and_then(|e| e.f("blowoffL1Choice")).unwrap_or(1.0) as u32,
            reflection_lp: et.child("ExhaustReflection").and_then(|e| e.f("LPCutoff")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarAudio {
    pub car: String,
    pub rpm_idle: f32,
    pub rpm_redline: f32,
    pub cylinders: u32,
    pub engine_volume: f32,
    pub transmission_volume: f32,
    pub shift_volume: f32,
    pub turbo_volume: f32,
    pub supercharger_volume: f32,
    pub global_volume: Option<Curve>,
    pub intake: Option<Emitter>,
    pub ambient: Option<Emitter>,
    pub exhaust: Option<Emitter>,
    /// `Front`, `Follow`, `Cockpit`, `Default`.
    pub views: BTreeMap<String, ViewMix>,
    /// Car-wide DSP + burble/backfire/shift data (empty in audio-1 installs).
    #[serde(default)]
    pub extras: EngineExtras,
}

impl CarAudio {
    /// `et` and `cmt` are the car's XML (either may be missing); `ht` loads a harmonic
    /// tuning file by name (case-insensitive lookup is the caller's job).
    pub fn resolve(
        car: &str,
        et: Option<&str>,
        cmt: Option<&str>,
        mut ht: impl FnMut(&str) -> Option<String>,
    ) -> Result<CarAudio> {
        let et = et.map(El::parse).transpose().with_context(|| format!("{car} ET"))?;
        let cmt = cmt.map(El::parse).transpose().with_context(|| format!("{car} CMT"))?;
        let et_ref = et.as_ref();
        let settings = et_ref.and_then(|e| e.child("EngineSettings"));
        let vols = et_ref.and_then(|e| e.child("VolumeAdjustments"));
        let vol = |k| vols.and_then(|v| v.f(k)).unwrap_or(0.5);

        let mut emitter = |section: &str, group: usize| -> Result<Option<Emitter>> {
            let Some(name) = et_ref.and_then(|e| e.path(&[section, "Upgrade"])).and_then(|u| u.attr("L0"))
            else {
                return Ok(None);
            };
            if name.eq_ignore_ascii_case("NA.xml") {
                return Ok(None);
            }
            let Some(xml) = ht(name) else { return Ok(None) };
            let mut em = match Emitter::parse(name, &El::parse(&xml).with_context(|| name.to_owned())?) {
                Some(e) => e,
                None => return Ok(None),
            };
            // The car's own ET overrides the bank's default DSP.
            if let Some(g) = et_ref.and_then(|e| e.path(&["HarmonicTunings", &format!("EmissionGroup{group}")])) {
                em.dsp = Dsp::parse(g);
            }
            Ok(Some(em))
        };
        let intake = emitter("EngineIntake", 0)?;
        let ambient = emitter("EngineAmbient", 1)?;
        let exhaust = emitter("Exhaust", 2)?;

        let mut views = BTreeMap::new();
        if let Some(basic) = cmt.as_ref().and_then(|c| c.path(&["Listener", "Basic"])) {
            for v in &basic.kids {
                let (Some(eg), Some(o)) = (v.child("EmissionGroupVolumes"), v.child("OtherVolumes")) else {
                    continue;
                };
                let d = ViewMix::default();
                views.insert(
                    v.name.clone(),
                    ViewMix {
                        intake: eg.f("EngineIntake").unwrap_or(d.intake),
                        ambient: eg.f("EngineAmbient").unwrap_or(d.ambient),
                        exhaust_l: eg.f("ExhaustL").unwrap_or(d.exhaust_l),
                        exhaust_r: eg.f("ExhaustR").unwrap_or(d.exhaust_r),
                        tire: o.f("TireVolume").unwrap_or(d.tire),
                        transmission: o.f("TransmissionVolume").unwrap_or(d.transmission),
                        turbo: o.f("TurboVolume").unwrap_or(d.turbo),
                        supercharger: o.f("SuperChargerVolume").unwrap_or(d.supercharger),
                        wind: o.f("CarWindVolume").unwrap_or(d.wind),
                    },
                );
            }
        }

        Ok(CarAudio {
            car: car.to_owned(),
            rpm_idle: settings.and_then(|s| s.f("audio_rpm_idle")).unwrap_or(900.0),
            rpm_redline: settings.and_then(|s| s.f("audio_rpm_redline")).unwrap_or(7000.0),
            cylinders: settings.and_then(|s| s.f("num_cylinders")).unwrap_or(4.0) as u32,
            engine_volume: vol("EngineVolume"),
            transmission_volume: vol("TransmissionVolume"),
            shift_volume: vol("ShiftVolume"),
            turbo_volume: vol("TurboVolume"),
            supercharger_volume: vol("SuperChargerVolume"),
            global_volume: et_ref.and_then(|e| e.child("GlobalCarVolume")).and_then(Curve::parse),
            intake,
            ambient,
            exhaust,
            views,
            extras: et_ref.map(EngineExtras::parse).unwrap_or_default(),
        })
    }

    pub fn banks(&self) -> impl Iterator<Item = &str> {
        [&self.intake, &self.ambient, &self.exhaust].into_iter().flatten().map(|e| e.bank.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_eval() {
        let c = Curve { coeff: [1.0, 0.0, 0.0, 0.0], pts: [[0.0, 0.3], [0.5, 0.14], [1.0, 0.08]] };
        let at = |rpm| c.eval(&Drive { rpm, ..Default::default() });
        assert!((at(0.0) - 0.3).abs() < 1e-6);
        assert!((at(0.25) - 0.22).abs() < 1e-6);
        assert!((at(0.75) - 0.11).abs() < 1e-6);
        assert!((at(2.0) - 0.08).abs() < 1e-6);
        // Degenerate x (all zero) evaluates to y0.
        let d = Curve { coeff: [0.0; 4], pts: [[0.0, 4328.0], [0.0, 20.0], [0.0, 20.0]] };
        assert_eq!(d.eval(&Drive { rpm: 1.0, ..Default::default() }), 4328.0);
    }
}
