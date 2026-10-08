//! `media/audio/radio/RadioSystem.xml` → [`RadioSystem`].
//!
//! Verified against the EU disc: 9 stations (`Radio1..3` = the three DJ stations,
//! `Radio4_Silent` = radio off, `Radio5..9_3d_*` = positional venue music), 127 music clips,
//! DJ dialogue pools per station, gameplay-event lines and festival updates. Attribute names
//! are not consistently cased on the disc (`fmodname` vs `fmodName`), so keys are matched
//! case-insensitively.

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};

/// The `<System>` element: levels (linear gain) and fade times (seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemParams {
    pub max_tracks_between_dj: u32,
    pub level_fade_up_time: f32,
    pub level_fade_up_time_for_event: f32,
    pub level_fade_down_time: f32,
    pub duck_fade_up_time: f32,
    pub duck_fade_down_time: f32,
    /// Music gain while the DJ talks.
    pub duck_value: f32,
    /// Fade of the "scale" fader (0 on the silent station, 1 otherwise).
    #[serde(default = "default_scale_fade")]
    pub scale_fade_up_time: f32,
    #[serde(default = "default_scale_fade")]
    pub scale_fade_down_time: f32,
    pub master_level: f32,
    pub music_level: f32,
    pub dialogue_level: f32,
    pub ident_level: f32,
    pub hud_new_track_delay_free_roam: f32,
    pub hud_new_track_delay_race: f32,
    pub station_change_fade_time_normal: f32,
    pub station_change_fade_time_user: f32,
}

fn default_scale_fade() -> f32 {
    0.3
}

/// A pool the game picks from at random, never repeating one of the last `no_repeat` picks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool<T> {
    pub no_repeat: usize,
    pub items: Vec<T>,
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Pool { no_repeat: 0, items: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicTrack {
    pub title: String,
    pub artist: String,
    /// Clip name in `Radio_Music.fsb`.
    pub clip: String,
    /// Relative pick weight by time of day: morning, daytime, evening, night (all 1 on the disc).
    pub likelihood: [f32; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dialogue {
    /// Clip name in the language's `Radio_VO_<LANG>.fsb`.
    pub clip: String,
    /// 0 = always eligible; 20..22 = street-race hub hints, dropped once that hub is visited.
    pub group: u32,
}

/// A line played when a named gameplay event fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLine {
    pub event: String,
    pub clip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    /// `Radio1` .. `Radio9_3d_Streetrace`.
    pub name: String,
    pub is_off: bool,
    /// Venue music played positionally in the world rather than through the car radio.
    pub is_3d: bool,
    pub volume: f32,
    pub reverb_wet_db: f32,
    pub pan_3d: f32,
    pub music_loops: bool,
    pub playlist: Pool<MusicTrack>,
    /// Station jingles (VO bank: they are localised).
    pub idents: Pool<String>,
    pub bookend_in: Pool<String>,
    pub bookend_out: Pool<String>,
    pub dj_regular: Pool<Dialogue>,
    /// Queued until the next DJ slot.
    pub dj_special: Vec<EventLine>,
    /// Played straight away (opening sequence).
    pub dj_immediate: Vec<EventLine>,
    pub festival_lead_in: Pool<String>,
    pub festival_lead_out: Pool<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioSystem {
    pub system: SystemParams,
    pub stations: Vec<Station>,
    /// Shared across the DJ stations, wrapped in the station's lead-in/lead-out.
    pub festival_updates: Vec<EventLine>,
}

/// Placeholder clip names that mean "nothing" (not in any bank).
pub fn is_blank(clip: &str) -> bool {
    clip.eq_ignore_ascii_case("DJ_Blank") || clip.eq_ignore_ascii_case("Ident_Blank")
}

/// Minimal element tree.
#[derive(Debug, Default)]
pub(crate) struct El {
    pub(crate) name: String,
    attrs: Vec<(String, String)>,
    pub(crate) children: Vec<El>,
}

impl El {
    pub(crate) fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
    pub(crate) fn f32(&self, key: &str, default: f32) -> Result<f32> {
        self.attr(key).map_or(Ok(default), |v| v.trim().parse().with_context(|| format!("{}@{key}={v}", self.name)))
    }
    fn req(&self, key: &str) -> Result<&str> {
        self.attr(key).with_context(|| format!("<{}> missing {key}", self.name))
    }
    pub(crate) fn child(&self, name: &str) -> Option<&El> {
        self.children.iter().find(|c| c.name == name)
    }
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
}

pub(crate) fn parse_tree(xml: &str) -> Result<El> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack = vec![El::default()];
    let open = |e: &BytesStart| -> Result<El> {
        let mut el = El { name: AsRef::<str>::as_ref(&e.name()).to_owned(), ..El::default() };
        for a in e.attributes() {
            let a = a?;
            // The disc has bare `&` in some artist names, which strict unescaping rejects.
            let value = match a.normalized_value(XmlVersion::Implicit1_0) {
                Ok(v) => v.into_owned(),
                Err(_) => a.value.as_ref()
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            };
            el.attrs.push((AsRef::<str>::as_ref(&a.key).to_owned(), value));
        }
        Ok(el)
    };
    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let el = open(&e)?;
                stack.last_mut().unwrap().children.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().unwrap();
                stack.last_mut().context("unbalanced XML")?.children.push(el);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    stack.pop().context("empty XML")
}

/// Lists default to `noRepeat` 2 (station constructor 0x82BDBC28).
fn no_repeat(el: &El) -> Result<usize> {
    Ok(el.f32("noRepeat", 2.0)? as usize)
}

fn name_pool(el: Option<&El>, item: &str) -> Result<Pool<String>> {
    let Some(el) = el else { return Ok(Pool::default()) };
    Ok(Pool {
        no_repeat: no_repeat(el)?,
        items: el.all(item).map(|c| c.req("fmodName").map(str::to_owned)).collect::<Result<_>>()?,
    })
}

fn event_lines(el: Option<&El>, item: &str) -> Result<Vec<EventLine>> {
    let Some(el) = el else { return Ok(Vec::new()) };
    el.all(item)
        .map(|c| Ok(EventLine { event: c.req("gameplayEvent")?.to_owned(), clip: c.req("fmodName")?.to_owned() }))
        .collect()
}

fn station(el: &El) -> Result<Station> {
    let name = el.req("name")?.to_owned();
    let playlist = el.child("Playlist").context("station without Playlist")?;
    let tracks = playlist
        .all("MusicTrack")
        .map(|t| {
            Ok(MusicTrack {
                title: t.req("name")?.to_owned(),
                artist: t.req("artist")?.to_owned(),
                clip: t.req("fmodname")?.to_owned(),
                likelihood: [
                    t.f32("likelihoodMorning", 1.0)?,
                    t.f32("likelihoodDaytime", 1.0)?,
                    t.f32("likelihoodEvening", 1.0)?,
                    t.f32("likelihoodNight", 1.0)?,
                ],
            })
        })
        .collect::<Result<_>>()?;
    let dj_regular = match el.child("DJRegularDialogue") {
        None => Pool::default(),
        Some(d) => Pool {
            no_repeat: no_repeat(d)?,
            items: d
                .all("SimpleDialogue")
                .map(|c| Ok(Dialogue { clip: c.req("fmodName")?.to_owned(), group: c.f32("group", 0.0)? as u32 }))
                .collect::<Result<_>>()?,
        },
    };
    Ok(Station {
        is_off: el.attr("isOffStation").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        is_3d: name.contains("_3d_"),
        volume: el.f32("volume", 1.0)?,
        reverb_wet_db: el.f32("reverbWetLevel", 0.0)?,
        pan_3d: el.f32("3dPanLevel", 0.0)?,
        music_loops: !el.attr("musicLoops").is_some_and(|v| v.eq_ignore_ascii_case("false")),
        playlist: Pool { no_repeat: no_repeat(playlist)?, items: tracks },
        idents: name_pool(el.child("Idents"), "Ident")?,
        bookend_in: name_pool(el.child("DJBookendDialogueIn"), "SimpleDialogue")?,
        bookend_out: name_pool(el.child("DJBookendDialogueOut"), "SimpleDialogue")?,
        dj_regular,
        dj_special: event_lines(el.child("DJSpecialDialogue"), "SpecialDialogue")?,
        dj_immediate: event_lines(el.child("DJImmediateDialogue"), "SpecialDialogue")?,
        festival_lead_in: name_pool(el.child("DJFestivalUpdateLeadIn"), "SimpleDialogue")?,
        festival_lead_out: name_pool(el.child("DJFestivalUpdateLeadOut"), "SimpleDialogue")?,
        name,
    })
}

pub fn parse(xml: &str) -> Result<RadioSystem> {
    let root = parse_tree(xml)?;
    let rs = root.child("RadioSystem").context("no <RadioSystem>")?;
    let s = rs.child("System").context("no <System>")?;
    let system = SystemParams {
        max_tracks_between_dj: s.f32("maxTracksBetweenDJ", 1.0)? as u32,
        level_fade_up_time: s.f32("levelFadeUpTime", 2.0)?,
        level_fade_up_time_for_event: s.f32("levelFadeUpTimeForEvent", 5.0)?,
        level_fade_down_time: s.f32("levelFadeDownTime", 3.0)?,
        duck_fade_up_time: s.f32("duckFadeUpTime", 0.5)?,
        duck_fade_down_time: s.f32("duckFadeDownTime", 0.1)?,
        duck_value: s.f32("duckValue", 0.3)?,
        scale_fade_up_time: s.f32("scaleFadeUpTime", 0.3)?,
        scale_fade_down_time: s.f32("scaleFadeDownTime", 0.3)?,
        master_level: s.f32("masterLevel", 1.0)?,
        music_level: s.f32("musicLevel", 1.0)?,
        dialogue_level: s.f32("dialogueLevel", 1.0)?,
        ident_level: s.f32("identLevel", 1.0)?,
        hud_new_track_delay_free_roam: s.f32("HUDNewTrackDisplayDelayFreeRoam", 2.0)?,
        hud_new_track_delay_race: s.f32("HUDNewTrackDisplayDelayRace", 4.0)?,
        station_change_fade_time_normal: s.f32("stationChangeFadeTimeNormal", 0.5)?,
        station_change_fade_time_user: s.f32("stationChangeFadeTimeUser", 0.25)?,
    };
    Ok(RadioSystem {
        system,
        stations: rs.all("RadioStation").map(station).collect::<Result<_>>()?,
        festival_updates: event_lines(rs.child("FestivalUpdates"), "FestivalUpdate")?,
    })
}

/// Display name of a DJ station (EN `MyProfile.str` / `Tips.str`: Bass Arena = Scott Tyler's
/// EDM station, Pulse = Holly Cruz, Rocks = Phoenix Fox; matches the DJ clip prefixes).
pub fn station_display_name(station: &str) -> Option<(&'static str, &'static str)> {
    match station {
        "Radio1" => Some(("Horizon Bass Arena", "Scott Tyler")),
        "Radio2" => Some(("Horizon Pulse", "Holly Cruz")),
        "Radio3" => Some(("Horizon Rocks", "Phoenix Fox")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fragment() {
        let xml = r#"<RadioSystem><System maxTracksBetweenDJ="1" duckValue="0.3"/>
            <RadioStation name="Radio1"><Playlist noRepeat="15">
              <MusicTrack name="Blue Monday" artist="New Order" fmodname="R1_BlueMonday" likelihoodNight="0.5"/>
            </Playlist><Idents noRepeat="6"><Ident fmodName="R1_Ident_01"/></Idents>
            <DJRegularDialogue noRepeat="40"><SimpleDialogue fmodName="A" group="20"/></DJRegularDialogue>
            </RadioStation>
            <RadioStation name="Radio4_Silent" isOffStation="true"><Playlist noRepeat="1"/></RadioStation>
            <FestivalUpdates><FestivalUpdate gameplayEvent="E" fmodName="F"/></FestivalUpdates></RadioSystem>"#;
        let r = parse(xml).unwrap();
        assert_eq!(r.stations.len(), 2);
        let s = &r.stations[0];
        assert_eq!(s.playlist.no_repeat, 15);
        assert_eq!(s.playlist.items[0].clip, "R1_BlueMonday");
        assert_eq!(s.playlist.items[0].likelihood, [1.0, 1.0, 1.0, 0.5]);
        assert_eq!(s.idents.items, ["R1_Ident_01"]);
        assert_eq!(s.dj_regular.items[0].group, 20);
        assert!(r.stations[1].is_off);
        assert_eq!(r.festival_updates[0].clip, "F");
    }
}
