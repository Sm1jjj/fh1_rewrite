//! granny <disc/media/tracks/colorado> [name filter] : every animated object (`.pgeo` type 4): models,
//! LOD meshes, vertex formats, draws, animations, curve formats, and how far each model's bones move
//! over the animation (max translation / rotation change of the posed bone matrices).

use std::collections::{BTreeMap, HashSet};

use fh1_formats::granny::{parse_anim_object, Mat4};

fn delta(a: &Mat4, b: &Mat4) -> (f32, f32) {
    let t = ((0..3).map(|r| (a[3][r] - b[3][r]).powi(2)).sum::<f32>()).sqrt();
    // Rotation change: largest column-vector difference of the 3x3 part.
    let r = (0..3).map(|c| (0..3).map(|k| (a[c][k] - b[c][k]).powi(2)).sum::<f32>().sqrt()).fold(0.0, f32::max);
    (t, r)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let filt = a.get(2).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let mut ar = fh1_formats::zip::Archive::open(std::path::Path::new(&a[1]).join("bin.zip")).expect("bin.zip");
    let mut seen = HashSet::new();
    let mut pgeos: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    pgeos.sort_by_key(|e| e.name.to_ascii_lowercase());
    for e in pgeos {
        let d = ar.read(&e).unwrap();
        if d.get(0x30..0x34) != Some(&[0, 0, 0, 4]) {
            continue;
        }
        let o = match parse_anim_object(&d) {
            Ok(o) => o,
            Err(err) => {
                println!("{}: {err}", e.name);
                continue;
            }
        };
        if !o.name.to_ascii_lowercase().contains(&filt) {
            continue;
        }
        // GRANNY_LIGHTS=1: dump the meshes' light attachments (raw words; floats where they look like floats).
        if std::env::var("GRANNY_LIGHTS").is_ok() {
            for (mi, m) in o.models.iter().enumerate() {
                for (lod, meshes) in m.lods.iter().enumerate() {
                    for (k, mesh) in meshes.iter().enumerate() {
                        for g in &mesh.lights {
                            println!("{} model {mi} ({}) lod {lod} mesh {k} bone {:?} threshold {}: tex {} anim {} uv {}", o.name, o.granny.models.get(mi).map_or("", |x| x.name.as_str()), mesh.bone, f32::from_bits(mesh.model_data), g.texture, g.anim_texture, g.uv_scale);
                            for l in &g.lights {
                                println!("    {l:?}");
                            }
                        }
                    }
                }
            }
            continue;
        }
        let mut curves: BTreeMap<String, usize> = BTreeMap::new();
        for an in &o.granny.animations {
            for tg in &an.track_groups {
                for t in &tg.tracks {
                    for c in [&t.orientation, &t.position, &t.scale_shear] {
                        *curves.entry(format!("{} d{}", c.format, c.degree)).or_default() += 1;
                        // How far Granny's B-spline is from the plain lerp between controls (the old evaluator).
                        if c.degree > 1 && !c.identity && c.knots.len() > 2 && c.dim > 0 {
                            let n = c.knots.len().min(c.controls.len() / c.dim);
                            let (k0, k1) = (c.knots[0], c.knots[n - 1]);
                            let mut dev = 0.0f32;
                            for s in 0..=200 {
                                let tt = k0 + (k1 - k0) * s as f32 / 200.0;
                                let i = c.knots[..n].partition_point(|&k| k <= tt).saturating_sub(1).min(n - 2);
                                let f = ((tt - c.knots[i]) / (c.knots[i + 1] - c.knots[i]).max(1e-9)).clamp(0.0, 1.0);
                                let b = c.evaluate(tt, Some(an.duration), c.dim == 4).unwrap_or_default();
                                let mut l: Vec<f32> = (0..c.dim).map(|d| c.controls[i * c.dim + d] * (1.0 - f) + c.controls[(i + 1) * c.dim + d] * f).collect();
                                if c.dim == 4 {
                                    let m = l.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                                    let sg = if l.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>() < 0.0 { -1.0 } else { 1.0 };
                                    l.iter_mut().for_each(|x| *x *= sg / m);
                                }
                                dev = dev.max(l.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max));
                            }
                            let e = curves.entry("~max|bspline-lerp| x1000".to_string()).or_default();
                            *e = (*e).max((dev * 1000.0) as usize);
                        }
                    }
                }
            }
        }
        // GRANNY_RATE=1: angular speed spread (deg/s) of degree-2 orientation tracks, B-spline vs lerp.
        if std::env::var("GRANNY_RATE").is_ok() {
            for an in &o.granny.animations {
                for tg in &an.track_groups {
                    for t in &tg.tracks {
                        let c = &t.orientation;
                        if c.degree != 2 || c.dim != 4 || c.knots.len() < 3 {
                            continue;
                        }
                        let n = c.knots.len().min(c.controls.len() / 4);
                        let lerp = |tt: f32| {
                            let i = c.knots[..n].partition_point(|&k| k <= tt).saturating_sub(1).min(n - 2);
                            let f = ((tt - c.knots[i]) / (c.knots[i + 1] - c.knots[i]).max(1e-9)).clamp(0.0, 1.0);
                            let (a, b) = (&c.controls[i * 4..i * 4 + 4], &c.controls[i * 4 + 4..i * 4 + 8]);
                            let sg = if a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() < 0.0 { -1.0 } else { 1.0 };
                            let l: Vec<f32> = (0..4).map(|d| a[d] * (1.0 - f) + sg * b[d] * f).collect();
                            let m = l.iter().map(|x| x * x).sum::<f32>().sqrt();
                            l.iter().map(|x| x / m).collect::<Vec<_>>()
                        };
                        let ang = |a: &[f32], b: &[f32]| 2.0 * a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>().abs().min(1.0).acos().to_degrees();
                        let (k0, k1) = (c.knots[0], c.knots[n - 1]);
                        let lp = if std::env::var("GRANNY_NOLOOP").is_ok() { None } else { Some(an.duration) };
                        let steps = 400;
                        let dt = (k1 - k0) / steps as f32;
                        let (mut bs, mut li) = (Vec::new(), Vec::new());
                        for s in 0..steps {
                            let (ta, tb) = (k0 + dt * s as f32, k0 + dt * (s + 1) as f32);
                            bs.push(ang(&c.evaluate(ta, lp, true).unwrap(), &c.evaluate(tb, lp, true).unwrap()) / dt);
                            li.push(ang(&lerp(ta), &lerp(tb)) / dt);
                        }
                        if std::env::var("GRANNY_RATE").as_deref() == Ok("v") {
                            let ce = |i: usize| c.controls[i * 4..i * 4 + 4].to_vec();
                            let q0 = c.evaluate(k0, None, true).unwrap();
                            let q1 = c.evaluate(k1, None, true).unwrap();
                            println!("    first/last control {:.1} deg apart, clamped curve start/end {:.1} deg; ctrl0 {:?} ctrl1 {:?} ctrl[n-2] {:?} ctrl[n-1] {:?}", ang(&ce(0), &ce(n - 1)), ang(&q0, &q1), ce(0), ce(1), ce(n - 2), ce(n - 1));
                            let im = bs.iter().enumerate().fold((0, 0.0f32), |m, (i, &x)| if x > m.1 { (i, x) } else { m });
                            println!("    max at t={:.3} of {:.3}..{:.3} (dur {:.3}); knots tail {:?}", k0 + dt * im.0 as f32, k0, k1, an.duration, &c.knots[n.saturating_sub(4)..n]);
                        }
                        let st = |v: &[f32]| (v.iter().copied().fold(f32::MAX, f32::min), v.iter().copied().fold(0.0, f32::max), v.iter().sum::<f32>() / v.len() as f32);
                        let (b, l) = (st(&bs), st(&li));
                        println!("  rate {} {}: {} knots, bspline min/max/mean {:.1}/{:.1}/{:.1} deg/s, lerp {:.1}/{:.1}/{:.1}", tg.name, t.name, n, b.0, b.1, b.2, l.0, l.1, l.2);
                    }
                }
            }
        }
        println!("{} ({}): {} textures, {} models, anims {:?}, curves {curves:?}", o.name, o.granny.source.rsplit('\\').next().unwrap_or(""), o.textures.len(), o.models.len(), o.granny.animations.iter().map(|a| (a.duration, a.track_groups.len())).collect::<Vec<_>>());
        let dur = o.granny.duration().max(1.0);
        for (mi, m) in o.models.iter().enumerate() {
            let gm = &o.granny.models[mi];
            let lods: Vec<String> = m.lods.iter().map(|l| l.iter().map(|x| format!("{}v/{}t f{} b{:?} d{}", x.vertices.len(), x.indices.len() / 3, x.format, x.bone, x.draws.len())).collect::<Vec<_>>().join(" ")).collect();
            // Motion: compare poses over the animation.
            let p0 = o.granny.pose(mi, 0.0);
            let (mut mt, mut mr) = (0.0f32, 0.0f32);
            for k in 1..=20 {
                let p = o.granny.pose(mi, dur * k as f32 / 20.0);
                for (a, b) in p0.iter().zip(&p) {
                    let (t, r) = delta(a, b);
                    mt = mt.max(t);
                    mr = mr.max(r);
                }
            }
            println!("  model {mi} '{}' {} bones; motion: move {mt:.2} m, rot {mr:.2}; LODs {lods:?}", gm.name, gm.bones.len());
        }
    }
}
