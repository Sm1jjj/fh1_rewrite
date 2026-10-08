//! `.bgf` (behaviour/graph file): header with 27 count words, the string table, then six sections
//! `u32 byte_size + body`: S1 objects, S2 per-behaviour ids, S3 tracks, S4 handlers, S5 (empty), S6 slides.
//! The file ends exactly after S6. VERIFIED on all 230 files; the parser enforces every count the
//! header states (props, tracks, keys, slides, actions, strings, string bytes).

use super::{typed, Prop, PropValue};
use crate::reader::{latin1, Reader};
use crate::{need, Error, Result};

pub const MAGIC: &[u8; 8] = b"AnarkBGF";

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// 0x00: 0x0104 (same tag as the .bsg).
    pub tag: u16,
    /// 0x02: 0x1A.
    pub header_word: u32,
    /// 3 on every file.
    pub version: u32,
    pub u12: u32,
    /// 1280×720 (800×450 in 6 scenes).
    pub width: u16,
    pub height: u16,
    pub u1a: u8,
    /// Largest scene id (= max .fbf id in 201/205).
    pub max_id: u32,
    pub u1f: u8,
    /// w0..w26, see the accessors. VERIFIED against the parsed data.
    pub words: [u32; 27],
}

impl Header {
    pub fn n_nodes(&self) -> u32 { self.words[1] }
    pub fn n_components(&self) -> u32 { self.words[2] }
    pub fn n_props(&self) -> u32 { self.words[3] }
    pub fn n_s2_entries(&self) -> u32 { self.words[4] }
    pub fn n_s2_ids(&self) -> u32 { self.words[5] }
    pub fn n_strings(&self) -> u32 { self.words[7] }
    pub fn n_tracks(&self) -> u32 { self.words[9] }
    pub fn n_keys(&self) -> u32 { self.words[10] }
    pub fn n_handler_objects(&self) -> u32 { self.words[11] }
    pub fn n_events(&self) -> u32 { self.words[12] }
    pub fn n_actions(&self) -> u32 { self.words[13] }
    pub fn n_slides(&self) -> u32 { self.words[15] }
    pub fn n_slide_entries(&self) -> u32 { self.words[16] }
    pub fn n_slide_props(&self) -> u32 { self.words[17] }
    pub fn n_slide_refs(&self) -> u32 { self.words[18] }
    pub fn string_bytes(&self) -> u32 { self.words[26] }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// Group, model or layer.
    Node,
    Camera,
    Light,
    Text,
    Material,
    Image,
    /// Group with its own timeline and slides (the scene root is one).
    Component,
    /// Behaviour instance; `name_hash` = ahash of the behaviour type (`EventFire`, contracts…).
    Behavior,
    Other(u8),
}

impl ObjectKind {
    pub fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Node,
            2 => Self::Camera,
            3 => Self::Light,
            4 => Self::Text,
            5 => Self::Material,
            6 => Self::Image,
            7 => Self::Component,
            8 => Self::Behavior,
            o => Self::Other(o),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Node => 1,
            Self::Camera => 2,
            Self::Light => 3,
            Self::Text => 4,
            Self::Material => 5,
            Self::Image => 6,
            Self::Component => 7,
            Self::Behavior => 8,
            Self::Other(o) => o,
        }
    }
}

/// Extra fields of a component record (kind 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentInfo {
    /// = number of this component's slides in S6 (VERIFIED).
    pub n_slides: u8,
    /// = Master Slide end, ms (VERIFIED).
    pub duration: u32,
}

/// S1 record. Its position in [`Bgf::objects`] is the object index used everywhere in the bgf.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// ahash(object name), or ahash(behaviour type) for behaviours.
    pub name_hash: u32,
    /// Parent object index, -1 = root; always < own index (VERIFIED).
    pub parent: i32,
    /// Scene id (= .fbf/.bsg id); 0 for behaviours.
    pub id: u32,
    pub kind: ObjectKind,
    /// 1, or 5 for components.
    pub b: u8,
    pub component: Option<ComponentInfo>,
    /// Master values of every defined or animated property.
    pub props: Vec<Prop>,
}

/// S2 entry: ids per behaviour record (purpose GUESS: runtime registration ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S2Entry {
    pub object: i32,
    pub ids: Vec<u32>,
}

/// Keyframe: value at `time` ms (slide-relative), with Bezier control values `c1`, `c2`.
/// `pad` = the two always-zero floats (fields 2 and 4, VERIFIED 0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    pub time: u32,
    pub value: f32,
    pub c1: f32,
    pub c2: f32,
    pub pad: [f32; 2],
}

/// S3 track: one animated property of one object.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub object: i32,
    /// 27-bit property hash (type bits are 0).
    pub key: u32,
    /// Always 1.
    pub interp: u8,
    /// 0/1, meaning unknown.
    pub flag: u8,
    pub keys: Vec<Key>,
}

impl Track {
    /// Value at slide time `t` ms: hold before the first / after the last key, else a 1-D cubic
    /// Bezier `(v_i, c1_i, c2_i, v_i+1)` linear in time. Layout VERIFIED; the curve is a strong GUESS
    /// (it reproduces the 69 % exactly-linear segments).
    pub fn eval(&self, t: f32) -> Option<f32> {
        let first = self.keys.first()?;
        if t <= first.time as f32 {
            return Some(first.value);
        }
        for w in self.keys.windows(2) {
            let (k0, k1) = (w[0], w[1]);
            if t <= k1.time as f32 {
                let span = k1.time as f32 - k0.time as f32;
                let s = if span > 0.0 { (t - k0.time as f32) / span } else { 1.0 };
                let u = 1.0 - s;
                return Some(
                    u * u * u * k0.value + 3.0 * u * u * s * k0.c1 + 3.0 * u * s * s * k0.c2 + s * s * s * k1.value,
                );
            }
        }
        self.keys.last().map(|k| k.value)
    }
}

pub const CMD_SET_PROPERTY: u32 = 0x7E49_06FD;
pub const CMD_GOTO_SLIDE: u32 = 0x1195_C23B;
pub const CMD_GOTO_TIME: u32 = 0x5B2F_A69C;

/// Action command. Only SET_PROPERTY and GOTO_SLIDE are VERIFIED from their args; GOTO_TIME is a
/// GUESS (arg1 = ms). 0x573BA2B8 / 0x4F15F91E / 0x6444A519 (play/pause/stop?) and 0x06875030 stay Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// arg1 = property key (type bits + 16), arg2 = typed value.
    SetProperty,
    /// arg1 = slide name ahash31.
    GotoSlide,
    /// arg1 = time ms.
    GotoTime,
    Unknown(u32),
}

impl Command {
    pub fn from_code(c: u32) -> Self {
        match c {
            CMD_SET_PROPERTY => Self::SetProperty,
            CMD_GOTO_SLIDE => Self::GotoSlide,
            CMD_GOTO_TIME => Self::GotoTime,
            o => Self::Unknown(o),
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Self::SetProperty => CMD_SET_PROPERTY,
            Self::GotoSlide => CMD_GOTO_SLIDE,
            Self::GotoTime => CMD_GOTO_TIME,
            Self::Unknown(c) => c,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub target: i32,
    pub context: i32,
    pub command: Command,
    pub arg1: u32,
    pub arg2: u32,
}

impl Action {
    /// For SET_PROPERTY: the typed value (`arg2` interpreted by the type bits of `arg1`).
    pub fn set_value(&self) -> Option<PropValue> {
        (self.command == Command::SetProperty).then(|| typed(self.arg1, self.arg2))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// ahash31 of the event name.
    pub hash31: u32,
    pub actions: Vec<Action>,
}

/// S4 record: the events one object (a behaviour, or a component) reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handler {
    pub object: i32,
    pub events: Vec<Event>,
}

/// Slide entry reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlideRef {
    /// 0 → S3 track index, 1 → global action index.
    pub kind: u8,
    /// 0 = inherited from / listed in the Master Slide, 1 = belongs to this slide.
    pub own: u8,
    pub index: u16,
}

impl SlideRef {
    pub fn track(&self) -> Option<usize> {
        (self.kind == 0).then_some(self.index as usize)
    }

    pub fn action(&self) -> Option<usize> {
        (self.kind == 1).then_some(self.index as usize)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlideEntry {
    pub object: i32,
    /// Master Slide: always 0. Other slides: 1 = the object is on this slide, 0 = hidden.
    pub active: u8,
    /// Static values this slide sets.
    pub props: Vec<Prop>,
    pub refs: Vec<SlideRef>,
}

/// S6 slide (Anark state). The first slide of every component is "Master Slide" (VERIFIED).
#[derive(Debug, Clone, PartialEq)]
pub struct Slide {
    pub name: String,
    /// S1 index of the owning component.
    pub component: i32,
    /// 9, 0, 17, 13, 8, 4, 12. GUESS: bit0 = play on enter, bits 2..4 = end mode
    /// (2 stop, 3 loop, 4 play through to next).
    pub flags: u8,
    pub start: u32,
    pub end: u32,
    pub entries: Vec<SlideEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bgf {
    pub header: Header,
    /// Per-occurrence list (not deduplicated), Latin-1.
    pub strings: Vec<String>,
    pub objects: Vec<Object>,
    pub s2: Vec<S2Entry>,
    pub tracks: Vec<Track>,
    pub handlers: Vec<Handler>,
    pub slides: Vec<Slide>,
}

impl Bgf {
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(6..14) != Some(MAGIC.as_slice()) {
            return Err(Error::BadMagic("AnarkBGF"));
        }
        let mut r = Reader::new(d, 0);
        let tag = r.u16("bgf tag")?;
        let header_word = r.u32("bgf tag")?;
        r.o = 0x0E;
        let header = Header {
            tag,
            header_word,
            version: r.u32("bgf header")?,
            u12: r.u32("bgf header")?,
            width: r.u16("bgf header")?,
            height: r.u16("bgf header")?,
            u1a: r.u8("bgf header")?,
            max_id: r.u32("bgf header")?,
            u1f: r.u8("bgf header")?,
            words: r.u32s("bgf header")?,
        };
        let mut strings = Vec::new();
        for _ in 0..header.n_strings() {
            strings.push(r.lstr("bgf string")?);
        }
        need(r.o == 0x8C + header.string_bytes() as usize, || "bgf string table size".into())?;

        let end = section(&mut r)?;
        let mut objects = Vec::new();
        while r.o < end {
            objects.push(object(&mut r)?);
        }
        need(r.o == end, || "bgf S1 overrun".into())?;

        let end = section(&mut r)?;
        let mut s2 = Vec::new();
        while r.o < end {
            let object = r.i32("bgf S2")?;
            let n = r.u32("bgf S2")?;
            let ids = (0..n).map(|_| r.u32("bgf S2")).collect::<Result<_>>()?;
            s2.push(S2Entry { object, ids });
        }
        need(r.o == end, || "bgf S2 overrun".into())?;

        let end = section(&mut r)?;
        let mut tracks = Vec::new();
        while r.o < end {
            tracks.push(track(&mut r)?);
        }
        need(r.o == end, || "bgf S3 overrun".into())?;

        let end = section(&mut r)?;
        let mut handlers = Vec::new();
        while r.o < end {
            handlers.push(handler(&mut r)?);
        }
        need(r.o == end, || "bgf S4 overrun".into())?;

        let end = section(&mut r)?;
        need(r.o == end, || "bgf S5 not empty".into())?;

        let end = section(&mut r)?;
        let mut slides = Vec::new();
        while r.o < end {
            slides.push(slide(&mut r)?);
        }
        need(r.o == end, || "bgf S6 overrun".into())?;
        need(r.o == d.len(), || format!("bgf: {} trailing bytes", d.len() - r.o))?;

        let bgf = Self { header, strings, objects, s2, tracks, handlers, slides };
        bgf.check_counts()?;
        Ok(bgf)
    }

    fn check_counts(&self) -> Result<()> {
        let h = &self.header;
        let props: usize = self.objects.iter().map(|o| o.props.len()).sum();
        let keys: usize = self.tracks.iter().map(|t| t.keys.len()).sum();
        let events: usize = self.handlers.iter().map(|h| h.events.len()).sum();
        let s2_ids: usize = self.s2.iter().map(|e| e.ids.len()).sum();
        let entries = || self.slides.iter().flat_map(|s| s.entries.iter());
        let components = self.objects.iter().filter(|o| o.kind == ObjectKind::Component).count();
        let checks = [
            ("components", components, h.n_components()),
            ("non-component objects", self.objects.len() - components, h.n_nodes()),
            ("S2 entries", self.s2.len(), h.n_s2_entries()),
            ("S2 ids", s2_ids, h.n_s2_ids()),
            ("slide entries", entries().count(), h.n_slide_entries()),
            ("slide props", entries().map(|e| e.props.len()).sum(), h.n_slide_props()),
            ("slide refs", entries().map(|e| e.refs.len()).sum(), h.n_slide_refs()),
            ("props", props, h.n_props()),
            ("tracks", self.tracks.len(), h.n_tracks()),
            ("keys", keys, h.n_keys()),
            ("slides", self.slides.len(), h.n_slides()),
            ("actions", self.actions().count(), h.n_actions()),
            ("handler objects", self.handlers.len(), h.n_handler_objects()),
            ("events", events, h.n_events()),
        ];
        for (what, got, want) in checks {
            need(got == want as usize, || format!("bgf {what}: header {want}, parsed {got}"))?;
        }
        Ok(())
    }

    /// Every action in file order; the position is the index used by kind-1 slide refs.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.handlers.iter().flat_map(|h| h.events.iter().flat_map(|e| e.actions.iter()))
    }

    pub fn object(&self, index: i32) -> Option<&Object> {
        self.objects.get(usize::try_from(index).ok()?)
    }

    /// Slides of component `index` (Master Slide first).
    pub fn slides_of(&self, index: i32) -> impl Iterator<Item = &Slide> {
        self.slides.iter().filter(move |s| s.component == index)
    }
}

/// Reads a section size and returns the section end offset.
fn section(r: &mut Reader) -> Result<usize> {
    let size = r.u32("bgf section size")? as usize;
    let end = r.o.checked_add(size).filter(|&e| e <= r.d.len());
    end.ok_or(Error::Truncated { what: "bgf section", at: r.o })
}

fn props(r: &mut Reader, what: &'static str) -> Result<Vec<Prop>> {
    let n = r.u32(what)?;
    let mut v = Vec::new();
    for _ in 0..n {
        v.push(Prop { key: r.u32(what)?, raw: r.u32(what)? });
    }
    Ok(v)
}

fn object(r: &mut Reader) -> Result<Object> {
    let name_hash = r.u32("bgf S1")?;
    let parent = r.i32("bgf S1")?;
    let id = r.u32("bgf S1")?;
    let kind = ObjectKind::from_code(r.u8("bgf S1")?);
    let b = r.u8("bgf S1")?;
    let component = if kind == ObjectKind::Component {
        Some(ComponentInfo { n_slides: r.u8("bgf S1")?, duration: r.u32("bgf S1")? })
    } else {
        None
    };
    let props = props(r, "bgf S1 props")?;
    Ok(Object { name_hash, parent, id, kind, b, component, props })
}

fn track(r: &mut Reader) -> Result<Track> {
    let object = r.i32("bgf S3")?;
    let key = r.u32("bgf S3")?;
    let interp = r.u8("bgf S3")?;
    let flag = r.u8("bgf S3")?;
    let n = r.u16("bgf S3")?;
    let mut keys = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let time = r.u32("bgf key")?;
        let [value, p0, c1, p1, c2] = r.f32s("bgf key")?;
        keys.push(Key { time, value, c1, c2, pad: [p0, p1] });
    }
    Ok(Track { object, key, interp, flag, keys })
}

fn handler(r: &mut Reader) -> Result<Handler> {
    let object = r.i32("bgf S4")?;
    let n_events = r.u32("bgf S4")?;
    let mut events = Vec::new();
    for _ in 0..n_events {
        let hash31 = r.u32("bgf S4 event")?;
        let n = r.u32("bgf S4 event")?;
        let mut actions = Vec::new();
        for _ in 0..n {
            actions.push(Action {
                target: r.i32("bgf action")?,
                context: r.i32("bgf action")?,
                command: Command::from_code(r.u32("bgf action")?),
                arg1: r.u32("bgf action")?,
                arg2: r.u32("bgf action")?,
            });
        }
        events.push(Event { hash31, actions });
    }
    Ok(Handler { object, events })
}

fn slide(r: &mut Reader) -> Result<Slide> {
    let raw = r.bytes(16, "bgf slide name")?;
    let name = latin1(raw.split(|&c| c == 0).next().unwrap_or_default());
    let component = r.i32("bgf slide")?;
    let flags = r.u8("bgf slide")?;
    let start = r.u32("bgf slide")?;
    let end = r.u32("bgf slide")?;
    let n = r.u32("bgf slide")?;
    let mut entries = Vec::new();
    for _ in 0..n {
        let object = r.i32("bgf slide entry")?;
        let active = r.u8("bgf slide entry")?;
        let props = props(r, "bgf slide props")?;
        let nr = r.u32("bgf slide refs")?;
        let mut refs = Vec::new();
        for _ in 0..nr {
            refs.push(SlideRef {
                kind: r.u8("bgf slide ref")?,
                own: r.u8("bgf slide ref")?,
                index: r.u16("bgf slide ref")?,
            });
        }
        entries.push(SlideEntry { object, active, props, refs });
    }
    Ok(Slide { name, component, flags, start, end, entries })
}
