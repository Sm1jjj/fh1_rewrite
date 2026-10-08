//! Present mode (2026-10-08 perf). Bevy's default FIFO waits for the next vblank, so on the user's high-refresh display a
//! 14.5 ms frame was shown for 5 refreshes (16.7 ms): the per-second frame times clustered at 13-14 and 17 ms. Mailbox
//! shows the newest finished frame at the next vblank without tearing and without holding the render thread, so frames
//! take their real time. `FH1_PRESENT=fifo` (default, as Bevy) | `mailbox` | `immediate` | `auto` | `ab` (Mailbox and FIFO
//! alternate every [`AB_SECS`] s; the recorder's `present_mode` column says which, for a same-run comparison). Bevy falls
//! back Mailbox -> Immediate -> Fifo when the surface lacks it and logs "PresentMode Mailbox requested but not available";
//! set FH1_PRESENT=fifo then if tearing shows. `FH1_VSYNC=0` (scenery/p2.rs, AutoNoVsync) still wins.

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::prelude::*;
use bevy::window::{PresentMode, PrimaryWindow};

const AB_SECS: f32 = 20.0;

/// The mode last set: 0 = not set by us, 1 = Mailbox, 2 = Fifo, 3 = Immediate, 4 = AutoVsync.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// For the recorder's `present_mode` column.
pub(crate) fn current_name() -> &'static str {
    match CURRENT.load(Ordering::Relaxed) {
        1 => "mailbox",
        2 => "fifo",
        3 => "immediate",
        4 => "auto",
        _ => "",
    }
}

fn code(m: PresentMode) -> u8 {
    match m {
        PresentMode::Mailbox => 1,
        PresentMode::Fifo => 2,
        PresentMode::Immediate => 3,
        PresentMode::AutoVsync => 4,
        _ => 0,
    }
}

pub(super) fn plugin(app: &mut App) {
    if std::env::var("FH1_VSYNC").as_deref() == Ok("0") {
        return;
    }
    let spec = std::env::var("FH1_PRESENT").unwrap_or_default().to_ascii_lowercase();
    let mode = match spec.as_str() {
        "mailbox" => PresentMode::Mailbox,
        "immediate" => PresentMode::Immediate,
        "auto" => PresentMode::AutoVsync,
        // FIFO = Bevy's default (2026-10-08 same-run A/B, user log 101726: FIFO 50.3 fps / p50 19.8 ms vs Mailbox 47.3 /
        // 21.0, render thread 20.8 vs 21.9 ms; the free-running GPU competed with the CPU-bound render thread).
        _ => PresentMode::Fifo,
    };
    app.insert_resource(Wanted { mode, ab: spec == "ab", t: 0.0 }).add_systems(Update, apply);
}

#[derive(Resource)]
struct Wanted {
    mode: PresentMode,
    ab: bool,
    t: f32,
}

/// Sets the primary window's mode once it exists (and back if anything else changes it, except FH1_VSYNC=0's).
fn apply(mut wanted: ResMut<Wanted>, real: Res<Time<Real>>, mut windows: Query<&mut Window, With<PrimaryWindow>>, mut logged: Local<bool>) {
    let Ok(mut w) = windows.single_mut() else { return };
    if wanted.ab {
        wanted.t += real.delta_secs();
        if wanted.t >= AB_SECS {
            wanted.t = 0.0;
            wanted.mode = if wanted.mode == PresentMode::Mailbox { PresentMode::Fifo } else { PresentMode::Mailbox };
            info!("present mode A/B: now {:?}", wanted.mode);
        }
    }
    if w.present_mode != wanted.mode {
        w.present_mode = wanted.mode;
    }
    CURRENT.store(code(wanted.mode), Ordering::Relaxed);
    if !*logged {
        *logged = true;
        info!("present mode: {:?} requested{} (FH1_PRESENT=fifo = the old vsync)", wanted.mode, if wanted.ab { ", A/B every 20 s" } else { "" });
    }
}
