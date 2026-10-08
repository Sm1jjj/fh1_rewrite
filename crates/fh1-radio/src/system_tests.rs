//! Tests for [`super`] on small synthetic data, one per behaviour in the spec.

use super::*;
use crate::config::{Dialogue, EventLine, MusicTrack, Pool, RadioSystem as Cfg, SystemParams};
use crate::install::Bank as ClipBank;
use std::collections::BTreeMap;

fn clip(secs: f64, sync: &[(&str, f64)]) -> Clip {
    Clip {
        file: String::new(),
        frames: (secs * 48000.0) as u64,
        rate: 48000,
        channels: 2,
        sync: sync.iter().map(|(n, s)| (n.to_string(), (s * 48000.0) as u64)).collect(),
    }
}

fn station(name: &str, tracks: &[&str], off: bool) -> Station {
    let ident = |s: &str| if off { "Ident_Blank".to_owned() } else { s.to_owned() };
    Station {
        name: name.into(),
        is_off: off,
        is_3d: false,
        volume: 1.0,
        reverb_wet_db: 0.0,
        pan_3d: 1.0,
        music_loops: false,
        playlist: Pool {
            no_repeat: 15,
            items: tracks
                .iter()
                .map(|t| MusicTrack { title: format!("T {t}"), artist: "A".into(), clip: t.to_string(), likelihood: [1.0; 4] })
                .collect(),
        },
        idents: Pool { no_repeat: 1, items: vec![ident("ID"), ident("ID2")] },
        bookend_in: Pool::default(),
        bookend_out: Pool::default(),
        dj_regular: Pool {
            no_repeat: 1,
            items: vec![
                Dialogue { clip: "DJ1".into(), group: 0 },
                Dialogue { clip: "DJ2".into(), group: 0 },
                Dialogue { clip: "DJ3".into(), group: 20 },
            ],
        },
        dj_special: vec![EventLine { event: "Barn".into(), clip: "SPECIAL".into() }],
        dj_immediate: vec![EventLine { event: "Now".into(), clip: "IMM".into() }],
        festival_lead_in: Pool { no_repeat: 1, items: vec!["LIN".into()] },
        festival_lead_out: Pool { no_repeat: 1, items: vec!["LOUT".into()] },
    }
}

fn data() -> Arc<RadioData> {
    let system = SystemParams {
        max_tracks_between_dj: 1,
        level_fade_up_time: 2.0,
        level_fade_up_time_for_event: 5.0,
        level_fade_down_time: 3.0,
        duck_fade_up_time: 0.5,
        duck_fade_down_time: 0.1,
        duck_value: 0.3,
        scale_fade_up_time: 0.3,
        scale_fade_down_time: 0.3,
        master_level: 1.0,
        music_level: 0.85,
        dialogue_level: 1.0,
        ident_level: 0.3,
        hud_new_track_delay_free_roam: 2.0,
        hud_new_track_delay_race: 4.0,
        station_change_fade_time_normal: 0.5,
        station_change_fade_time_user: 0.25,
    };
    let mut music = ClipBank::new();
    for n in ["A1", "A2", "A3", "A4", "B1", "B2", "S1"] {
        music.insert(n.into(), clip(100.0, &[("SongStart", 10.0), ("EventStart", 40.0), ("IdentStart", 95.0)]));
    }
    let mut vo = ClipBank::new();
    vo.insert("ID".into(), clip(9.0, &[("StartNextTrack", 8.0)]));
    vo.insert("ID2".into(), clip(9.0, &[("StartNextTrack", 8.0)]));
    vo.insert("Ident_Blank".into(), clip(2.0, &[("StartNextTrack", 1.4)]));
    for (n, l) in [("DJ1", 12.0), ("DJ2", 12.0), ("DJ3", 12.0), ("SPECIAL", 15.0), ("IMM", 5.0), ("LIN", 6.0), ("LOUT", 6.0), ("FEST", 20.0)] {
        vo.insert(n.into(), clip(l, &[]));
    }
    let radio = Cfg {
        system,
        stations: vec![
            station("Radio1", &["A1", "A2", "A3", "A4"], false),
            station("Radio2", &["B1", "B2"], false),
            station("Radio4_Silent", &["S1"], true),
        ],
        festival_updates: vec![EventLine { event: "Fest".into(), clip: "FEST".into() }],
    };
    Arc::new(RadioData { radio, music, vo: BTreeMap::from([("EN".to_string(), vo)]), snapshots: Default::default(), mods: Vec::new() })
}

fn run(rs: &mut RadioSystem, secs: f64) {
    for _ in 0..(secs / TICK).round() as usize {
        rs.update(false);
    }
}

/// Updates until `f` holds (at most `secs`).
fn until(rs: &mut RadioSystem, secs: f64, f: impl Fn(&RadioSystem) -> bool) -> f64 {
    for _ in 0..(secs / TICK) as usize {
        rs.update(false);
        if f(rs) {
            return rs.clock();
        }
    }
    panic!("condition not reached in {secs} s");
}

#[test]
fn lcg_matches_the_game() {
    let mut r = Lcg::new(0);
    // s = 0 * 0x41C64E6D + 0x3039 = 0x3039, whose high bits are 0.
    assert_eq!(r.sample(), 0.0);
    let s2 = 0x3039u32.wrapping_mul(0x41C6_4E6D).wrapping_add(0x3039);
    assert_eq!(r.sample(), ((s2 >> 16) & 0x7FFF) as f64);
}

#[test]
fn playlist_is_a_fixed_cycle() {
    let mut rs = RadioSystem::new(data(), "EN", 7);
    let mut order = Vec::new();
    for _ in 0..12 {
        let t = rs.pick_track(0).unwrap();
        rs.play_track(0.0, t, 1);
        order.push(t.1);
    }
    let mut first = order[..4].to_vec();
    assert_eq!(order[4..8], first[..], "the first order repeats: {order:?}");
    assert_eq!(order[8..12], first[..]);
    first.sort();
    assert_eq!(first, [0, 1, 2, 3], "first pass is a permutation: {order:?}");
}

#[test]
fn segue_skips_the_intro_and_dj_talks_over_the_song() {
    let mut rs = RadioSystem::new(data(), "EN", 3);
    rs.start_free_roam(0.0, false);
    let first = rs.music.clone().unwrap();
    // First visit: somewhere in the middle 80% of SongStart..IdentStart.
    assert!(first.pos_ms >= 18_499.0 && first.pos_ms <= 86_501.0, "{}", first.pos_ms);
    // The ident starts at IdentStart, over the outro.
    until(&mut rs, 200.0, |r| r.ident.is_some());
    assert!(rs.music.as_ref().unwrap().pos_ms >= 95_000.0);
    // At the ident's StartNextTrack: the next song from SongStart, and the DJ at the same time.
    until(&mut rs, 20.0, |r| r.music.as_ref().is_some_and(|m| m.id != first.id));
    let m = rs.music.as_ref().unwrap();
    assert!((m.pos_ms - 10_000.0).abs() < 1.0, "song starts at SongStart, not 0: {}", m.pos_ms);
    let dj = rs.dj.as_ref().expect("DJ line starts with the song");
    assert!(dj.pos_ms < 1.0 && dj.clip.starts_with("DJ"));
    assert_eq!(rs.snapshot, snapshots::RADIO_DJ_SPEAKING);
    // DJ end: RadioNormal, then a HUD post for the new song with the free-roam delay.
    rs.take_hud_posts();
    until(&mut rs, 20.0, |r| r.dj.is_none());
    run(&mut rs, 0.1);
    assert_eq!(rs.snapshot, snapshots::RADIO_NORMAL);
    let posts = rs.take_hud_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].delay, 2.0);
    assert!(!posts[0].show_station && posts[0].title.starts_with("T "));
}

#[test]
fn dj_option_off_drops_idents_and_waits_for_the_end() {
    let mut rs = RadioSystem::new(data(), "EN", 5);
    rs.start_free_roam(0.0, true);
    rs.set_dj_option(false);
    let first = rs.music.as_ref().unwrap().id;
    let t = until(&mut rs, 200.0, |r| r.ident.is_some());
    // Nothing at IdentStart: the blank ident only starts when the song ends.
    assert!(rs.music.is_none());
    assert_eq!(rs.ident.as_ref().unwrap().clip, "Ident_Blank");
    until(&mut rs, 5.0, |r| r.music.is_some());
    assert_ne!(rs.music.as_ref().unwrap().id, first);
    assert!(rs.clock() - t < 1.6);
    assert!(rs.dj.is_none());
}

#[test]
fn dpad_switch_fades_resumes_and_posts_the_logo() {
    let mut rs = RadioSystem::new(data(), "EN", 9);
    rs.start_free_roam(0.0, true);
    run(&mut rs, 3.0);
    let r1_track = rs.track().unwrap();
    rs.take_hud_posts();
    assert_eq!(rs.dpad(false), Some(1));
    // 1 s cooldown.
    assert_eq!(rs.dpad(false), None);
    // Radio1 while fading out (0.5 s), then Radio2.
    run(&mut rs, 0.3);
    assert_eq!(rs.station(), 0);
    run(&mut rs, 0.3);
    assert_eq!(rs.station(), 1);
    run(&mut rs, 0.06);
    let posts = rs.take_hud_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!((posts[0].station, posts[0].delay, posts[0].show_station), (1, 0.0, true));
    // Back to Radio1 resumes its song.
    run(&mut rs, 1.0);
    rs.dpad(true);
    run(&mut rs, 0.6);
    assert_eq!(rs.station(), 0);
    assert_eq!(rs.track(), Some(r1_track));
}

#[test]
fn off_station_posts_empty_texts_and_is_silent() {
    let mut rs = RadioSystem::new(data(), "EN", 2);
    rs.start_free_roam(0.0, true);
    rs.select_external(2, true, 2);
    run(&mut rs, 1.2);
    assert_eq!(rs.station(), 2);
    assert!(rs.paused());
    let p = rs.take_hud_posts().into_iter().last().unwrap();
    assert_eq!((p.station, p.title.as_str(), p.artist.as_str(), p.show_station), (2, "", "", true));
}

#[test]
fn festival_block_order() {
    let mut rs = RadioSystem::new(data(), "EN", 4);
    rs.start_free_roam(0.0, true);
    assert!(rs.trigger_festival("Fest", false));
    until(&mut rs, 200.0, |r| r.lead.is_some());
    // Lead-in with no music: the old song was stopped and the next not started.
    assert_eq!(rs.lead.as_ref().unwrap().clip, "LIN");
    assert!(rs.music.is_none());
    until(&mut rs, 10.0, |r| r.dj.is_some());
    assert_eq!(rs.dj.as_ref().unwrap().clip, "FEST");
    assert_eq!(rs.snapshot, snapshots::RADIO_FESTIVAL_UPDATE);
    // After the update: the next song from SongStart with the lead-out over it.
    until(&mut rs, 25.0, |r| r.dj.is_none());
    assert!((rs.music.as_ref().unwrap().pos_ms - 10_000.0).abs() < 50.0);
    assert_eq!(rs.lead.as_ref().unwrap().clip, "LOUT");
}

#[test]
fn special_beats_regular_and_immediate_needs_dj_disabled() {
    let mut rs = RadioSystem::new(data(), "EN", 6);
    rs.start_free_roam(0.0, true);
    rs.trigger_special("Barn", false);
    until(&mut rs, 200.0, |r| r.dj.is_some());
    assert_eq!(rs.dj.as_ref().unwrap().clip, "SPECIAL");
    assert!(!rs.trigger_immediate("Now", true));
    // Playback takes the first non-empty slot (festival, special, regular, immediate), so the
    // immediate line is tried once the special has finished.
    until(&mut rs, 30.0, |r| r.dj.is_none());
    rs.enable_dj(false);
    assert!(rs.trigger_immediate("Now", true));
    assert_eq!(rs.dj.as_ref().unwrap().clip, "IMM");
}

#[test]
fn disabled_groups_are_never_picked() {
    let mut rs = RadioSystem::new(data(), "EN", 8);
    rs.enable_dialogue_group(20, false);
    for _ in 0..50 {
        let l = rs.pick_dialogue(|s| &mut s.regular, |s| s.dj_regular.items.iter().map(|d| d.clip.clone()).collect());
        assert_ne!(l.as_deref(), Some("DJ3"));
    }
}

#[test]
fn duck_follows_the_voice() {
    let mut rs = RadioSystem::new(data(), "EN", 1);
    rs.start_free_roam(0.0, true);
    run(&mut rs, 3.0);
    let full = rs.volumes.music;
    for _ in 0..10 {
        rs.update(true);
    }
    assert!((rs.volumes.music / full - 0.3).abs() < 0.02, "ducked to duckValue within 0.1 s");
    for _ in 0..5 {
        rs.update(false);
    }
    let mid = rs.volumes.music / full;
    assert!(mid > 0.3 && mid < 1.0, "recovers at the 0.5 s rate: {mid}");
}
