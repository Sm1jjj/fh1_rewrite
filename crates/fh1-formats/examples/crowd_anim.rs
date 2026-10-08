//! Decode spectator animations and sanity-check them.
//! `cargo run --release -p fh1-formats --example crowd_anim -- <file.anim.bin | dir>`
#[path = "../src/crowd/anim.rs"]
#[allow(dead_code)]
mod anim;

use std::path::{Path, PathBuf};

fn len3(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Angle in degrees between two unit quaternions.
fn angle(a: [f32; 4], b: [f32; 4]) -> f32 {
    let d = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]).abs().min(1.0);
    2.0 * d.acos().to_degrees()
}

const MAX_STEP_DEG: f32 = 60.0;

/// Returns true when every check passed.
fn check(path: &Path) -> Result<bool, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let (skel, clip) = anim::parse(&data)?;
    let nb = skel.bones.len();
    let name = path.file_name().unwrap().to_string_lossy();

    // 1. Unit quaternions.
    let mut worst_norm = 0.0f32;
    for f in &clip.rotations {
        for q in f {
            let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
            worst_norm = worst_norm.max((n - 1.0).abs());
        }
    }
    // 2. Smoothness: largest rotation step between consecutive frames (and the bone it happens on).
    //    Fast cheers legitimately reach ~55 deg/frame on the forearm (a hinge-axis snap with smooth ramps
    //    either side). "Reversals" = frame f far from both neighbours while the neighbours agree: these
    //    occur at the turning points of fist pumps / waves (informational). A mis-aligned bitstream would
    //    scramble every later channel of the frame, so a frame with reversals on 4+ bones at once fails.
    let (mut step, mut step_bone, mut step_frame) = (0.0f32, 0, 0);
    let mut trans_step = 0.0f32;
    let mut spikes = 0usize;
    let mut glitch_frames = 0usize;
    for f in 1..clip.frames as usize {
        let mut frame_spikes = 0;
        for b in 0..nb {
            let a = angle(clip.rotations[f - 1][b], clip.rotations[f][b]);
            if a > step {
                (step, step_bone, step_frame) = (a, b, f);
            }
            trans_step = trans_step.max(len3(sub(clip.translations[f][b], clip.translations[f - 1][b])));
            if f + 1 < clip.frames as usize {
                let next = angle(clip.rotations[f][b], clip.rotations[f + 1][b]);
                let across = angle(clip.rotations[f - 1][b], clip.rotations[f + 1][b]);
                if a > 20.0 && next > 20.0 && across < 0.25 * a.min(next) {
                    spikes += 1;
                    frame_spikes += 1;
                }
            }
        }
        if frame_spikes >= 4 {
            glitch_frames += 1;
        }
    }
    // 3. Skeleton: bind locals summed down the chain = stored world bind positions.
    let mut bind_err = 0.0f32;
    let mut acc: Vec<[f32; 3]> = Vec::with_capacity(nb);
    for b in &skel.bones {
        let p = match b.parent {
            Some(p) => {
                let q = acc[p as usize];
                [q[0] + b.local[0], q[1] + b.local[1], q[2] + b.local[2]]
            }
            None => b.local,
        };
        bind_err = bind_err.max(len3(sub(p, b.world)));
        acc.push(p);
    }
    // 4. Frame 0, identity rotations, locals = bind + decoded offset: bones whose chain (self and all
    //    ancestors) has no translation offset at frame 0 must land on the stored world bind position.
    //    The largest offset (root drop of the ground sitters, etc.) is reported separately.
    let mut t0_err = 0.0f32;
    let mut t0_checked = 0usize;
    let mut max_offset = 0.0f32;
    let mut acc: Vec<([f32; 3], bool)> = Vec::with_capacity(nb);
    for (i, b) in skel.bones.iter().enumerate() {
        let t = clip.local_translation(&skel, 0, i);
        let off = len3(clip.translations[0][i]);
        max_offset = max_offset.max(off);
        let (p, clean) = match b.parent {
            Some(p) => {
                let (q, c) = acc[p as usize];
                ([q[0] + t[0], q[1] + t[1], q[2] + t[2]], c && off == 0.0)
            }
            None => (t, off == 0.0),
        };
        if clean {
            t0_err = t0_err.max(len3(sub(p, b.world)));
            t0_checked += 1;
        }
        acc.push((p, clean));
    }
    // 5. Full pose: frame 0 deviation from bind, loop closure, height range and hand height.
    let pose0 = clip.world_pose(&skel, 0);
    let pose_dev = (0..nb).map(|i| len3(sub(pose0[i].1, skel.bones[i].world))).fold(0.0f32, f32::max);
    let last = clip.frames as usize - 1;
    let loop_err = (0..nb).map(|b| angle(clip.rotations[0][b], clip.rotations[last][b])).fold(0.0f32, f32::max);
    let find = |s: &str| skel.bones.iter().position(|b| b.name.ends_with(s));
    let (head, lh, rh) = (find("_head"), find("armL_hand"), find("armR_hand"));
    let (mut min_y, mut max_y, mut max_hand) = (f32::MAX, f32::MIN, f32::MIN);
    let mut head_y = (f32::MAX, f32::MIN);
    for f in 0..clip.frames as usize {
        let p = clip.world_pose(&skel, f);
        for (_, pos) in &p {
            min_y = min_y.min(pos[1]);
            max_y = max_y.max(pos[1]);
        }
        for h in [lh, rh].into_iter().flatten() {
            max_hand = max_hand.max(p[h].1[1]);
        }
        if let Some(h) = head {
            head_y = (head_y.0.min(p[h].1[1]), head_y.1.max(p[h].1[1]));
        }
    }

    let ok_norm = worst_norm < 1e-3;
    let ok_step = step < MAX_STEP_DEG && glitch_frames == 0;
    let ok_bind = bind_err < 1e-3;
    let ok_t0 = t0_err < 1e-4;
    let ok = ok_norm && ok_step && ok_bind && ok_t0;
    println!(
        "{name:28} {:4} frames {:6.3}s bones {nb:2} | |q|-1 {worst_norm:.1e} {} | max step {step:5.2} deg (bone {step_bone} {}, frame {step_frame}) reversals {spikes} glitch frames {glitch_frames} {} | trans step {:.3} m | bind sum err {bind_err:.1e} {} | f0 bind check {t0_checked} bones err {t0_err:.1e} {} (max offset {max_offset:.3} m) | f0 pose vs bind {pose_dev:.3} m | loop {loop_err:5.2} deg | y {min_y:.2}..{max_y:.2} head {:.2}..{:.2} hand max {}",
        clip.frames,
        clip.duration,
        if ok_norm { "ok" } else { "FAIL" },
        skel.bones[step_bone].name,
        if ok_step { "ok" } else { "FAIL" },
        trans_step,
        if ok_bind { "ok" } else { "FAIL" },
        if ok_t0 { "ok" } else { "FAIL" },
        head_y.0,
        head_y.1,
        if max_hand > f32::MIN { format!("{max_hand:.2}") } else { "-".into() },
    );
    Ok(ok)
}

fn main() {
    let arg = std::env::args().nth(1).expect("usage: crowd_anim <file.anim.bin | dir>");
    let p = PathBuf::from(arg);
    let mut files: Vec<PathBuf> = if p.is_dir() {
        std::fs::read_dir(&p)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|f| f.to_string_lossy().ends_with(".anim.bin"))
            .collect()
    } else {
        vec![p]
    };
    files.sort();
    let (mut pass, mut fail, mut errors) = (0, 0, 0);
    for f in &files {
        match check(f) {
            Ok(true) => pass += 1,
            Ok(false) => fail += 1,
            Err(e) => {
                errors += 1;
                println!("{}: ERROR {e}", f.display());
            }
        }
    }
    if let Some(f) = files.first() {
        if let Ok((skel, _)) = std::fs::read(f).map_err(|e| e.to_string()).and_then(|d| anim::parse(&d)) {
            let names: Vec<&str> = skel.bones.iter().map(|b| b.name.as_str()).collect();
            println!("bones of {}: {names:?}", f.file_name().unwrap().to_string_lossy());
        }
    }
    println!("{} files: {pass} pass all checks, {fail} fail a check, {errors} parse errors", files.len());
}
