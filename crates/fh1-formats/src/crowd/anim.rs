//! Spectator animations: `media/Spectators.zip/anims/*.anim.bin` (56 clips, each with its own skeleton).
//!
//! VERIFIED on all 56 files on the EU disc (every region below is consumed exactly, up to 4-byte padding,
//! and every frame's bitstream ends inside its frame stride). Loader 0x82E24780, codec table 0x832BA1C8,
//! decoder 0x82E2ACA0 -> 0x82E2A500 (frame addresses) -> 0x82E2A880 (per-track control walk) ->
//! 0x82E2A5C8 (per-channel decode) -> 0x82E2A440 (bit reader); quaternion kernel 0x82E25AC8.
//!
//! **Container.** A `"CAFF21.11.05.0034"` file (same family as `caff` textures) with one `.data` section;
//! u32 @0x48 is the file offset of the asset (0x190 in all files). All offsets below are relative to the
//! asset start (pointers are stored pre-relocation, i.e. as asset offsets; the CAFF relocation list after
//! the data names exactly these pointer fields).
//!
//! **Asset header** (big-endian):
//! - @0x00 `"animation\0"`, @0x0A version `"00.00.00.0036\0"` (the loader rejects any other version).
//! - @0x18 u16 unknown (3..70, varies per clip; UNVERIFIED meaning), @0x1C f32 duration in seconds.
//! - @0x20 u16 bone count (19 standing, 15 sitting), @0x22 u16 frame count. Every clip is sampled at
//!   30 Hz: `frames = duration * 30 + 1` (VERIFIED on all 56).
//! - @0x24 u32 -> track group #1 (`"QUAT_BITSTREAM"`), @0x2C -> skeleton, @0x30 -> u16 bone map
//!   (identity 0..bones in every file), @0x34 -> bone-name table, @0x48 -> track group #2
//!   (`"BITSTREAM"`; present but empty, channel-descriptor pointer 0, in every file).
//!   @0x38 u16 = 10 and @0x44 f32 = 1.0 in every file (meaning UNVERIFIED).
//!
//! **Track group**: name (NUL-padded, up to 0x16 bytes); u16 @0x16 is filled at load time with the codec
//! index into the table at 0x832BA1C8 (QUAT_BITSTREAM 0, BITSTREAM 1, QUAT_UNCOMPRESSED, UNCOMPRESSED,
//! `*32` variants, QUAT_VARIABLE, VARIABLE); u32 @0x18 -> channel descriptor (0 = group unused).
//!
//! **Channel descriptor** (follows the group header): f32 @0 (1.0), u32 @4 -> per-channel f32 scale,
//! @8 -> per-channel f32 default, @0xC -> control bytes, @0x10 -> constants/bases (s16; s32 when wide),
//! @0x14 -> bit-width nibbles; u16 @0x18 constants size in bytes, @0x1A width-table size in bytes,
//! @0x1C frame stride in bytes, @0x1E channels per track (9), @0x20 output floats per track (12),
//! @0x22 track count (= bone count, track i animates bone i), @0x24 frame count; u8 @0x26 wide flag
//! (0 in every file). Frame `f`'s bitstream starts at `@0x10 + size@0x18 + size@0x1A + f * stride`.
//!
//! The 9 channels of a track are: 0-2 rotation quaternion x, y, z; 3-5 translation x, y, z; 6-8 scale
//! x, y, z. Scales are 1/16383 for the rotation channels and 1/128 (metres) for the others; defaults are
//! 0 for rotation/translation and 1 for scale (identical in every file).
//!
//! **Control bytes**, one or more per track. Each byte covers 3 channels: bit `0x80 >> k` = channel k is
//! present (else it takes its default); if present, bit `0x10 >> k` = constant (one value from the
//! constants stream, times the scale), else animated (one base from the constants stream and one bit
//! width from the nibble stream, width = nibble + 1, low nibble first; per frame the value is
//! `(bits + base) * scale`). The low 2 bits of a track's first byte (channels 0-2) say what follows:
//! 0 = nothing (channels 3-8 default), 1 = one byte for channels 6-8, 2 = one byte for channels 3-5,
//! 3 = two bytes (3-5 then 6-8). The constants and widths are consumed in channel order across all
//! tracks; the per-frame bitstream holds only the animated channels, in the same order.
//!
//! **Bit reader** (0x82E2A440): the stream is read as big-endian 32-bit words, LSB first: a byte address
//! `a` starts at word `a & !3`, bit `(a & 3) * 8`; `n` bits = `(w0 >> bit | w1 << (32 - bit)) & mask`.
//! (Frame strides are not multiples of 4, so this exact rule matters.)
//!
//! **Quaternion** (0x82E25AC8): `w = sqrt(1 - x² - y² - z²)` when positive, else 0 (`w` is never
//! negative). Largest `x²+y²+z²` over all files is 0.913, so every rotation is a proper unit quaternion.
//!
//! **Skeleton**: `bones` records of 52 bytes: f32x3 local bind translation, f32x3 world (model-space)
//! bind translation, 16 bytes of uninitialised exporter memory (path fragments; there is no bind
//! rotation), u16 parent, u16 first child, u16 next sibling, u16 own index, u16 0xFFFF, u16 0
//! (0xFFFF = none). Bind rotations are identity: `world = sum of locals` down the chain (VERIFIED,
//! <1e-5 m). +Y up, the figure faces +Z (hands move to +Z when cheering), metres; pelvis ~0.9 m.
//! **Names**: u32 count, u32 -> entries of (u32 -> NUL-terminated name, u32 0, u32 bone index), then a
//! hash table (unused here). Standing: `Bip001` (pelvis), `sk1_Bone_root`, legs, spine, `Bip001_Neck`,
//! arms, `sk1_Bone_head`.
//!
//! **Pose** (VERIFIED by plausibility: `Walkers_Male_01` gives an alternating gait with the feet
//! swinging ±0.35 m along Z and contralateral arm swing; cheers raise the hands to chest/head height):
//! local rotation = the decoded quaternion (Hamilton convention, `world_rot = parent_rot * local`),
//! local translation = bind local + decoded translation (the translation channels are offsets: they
//! default to 0 and are only present on the pelvis/root/spine/neck/shoulders, a few cm at most).
//! Clips loop: the last frame equals the first in the looping clips.
//!
//! Runtime interpolation between frames (0x82E26560 family: step / linear / cubic by a mode flag) is
//! not reproduced; sample frames and nlerp/slerp.
//! UNVERIFIED / not decoded: header u16 @0x18, the wide (32-bit) mode (no file uses it; implemented
//! from the decoder's branches), the other codecs (UNCOMPRESSED / VARIABLE: unused by these files).

use std::fmt;

/// Error from [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimError(pub String);

impl fmt::Display for AnimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "anim.bin: {}", self.0)
    }
}

impl std::error::Error for AnimError {}

impl From<AnimError> for String {
    fn from(e: AnimError) -> String {
        e.to_string()
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, AnimError> {
    Err(AnimError(msg.into()))
}

#[derive(Debug, Clone)]
pub struct Bone {
    pub name: String,
    pub parent: Option<u16>,
    pub first_child: Option<u16>,
    pub next_sibling: Option<u16>,
    /// Bind translation relative to the parent (bind rotations are identity).
    pub local: [f32; 3],
    /// Bind translation in model space (+Y up, faces +Z, metres).
    pub world: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct Skeleton {
    pub bones: Vec<Bone>,
}

#[derive(Debug, Clone)]
pub struct Clip {
    pub frames: u32,
    /// Seconds per frame (1/30 in every file).
    pub frame_time: f32,
    /// Header duration in seconds (= `(frames - 1) * frame_time`).
    pub duration: f32,
    /// `[frame][bone]` local rotation quaternions `[x, y, z, w]` (bind rotation is identity).
    pub rotations: Vec<Vec<[f32; 4]>>,
    /// `[frame][bone]` translation offsets added to the bone's bind `local` translation (mostly zero).
    pub translations: Vec<Vec<[f32; 3]>>,
    /// `[frame][bone]` scale (always 1 in the shipped files).
    pub scales: Vec<Vec<[f32; 3]>>,
}

impl Clip {
    /// Local translation of `bone` at `frame`: bind local + decoded offset.
    pub fn local_translation(&self, skel: &Skeleton, frame: usize, bone: usize) -> [f32; 3] {
        let b = skel.bones[bone].local;
        let t = self.translations[frame][bone];
        [b[0] + t[0], b[1] + t[1], b[2] + t[2]]
    }

    /// Model-space `(rotation, position)` of every bone at `frame` (parents precede children).
    pub fn world_pose(&self, skel: &Skeleton, frame: usize) -> Vec<([f32; 4], [f32; 3])> {
        let mut out: Vec<([f32; 4], [f32; 3])> = Vec::with_capacity(skel.bones.len());
        for (i, bone) in skel.bones.iter().enumerate() {
            let q = self.rotations[frame][i];
            let t = self.local_translation(skel, frame, i);
            let pose = match bone.parent {
                Some(p) if (p as usize) < out.len() => {
                    let (pq, pp) = out[p as usize];
                    let r = quat_rotate(pq, t);
                    (quat_mul(pq, q), [pp[0] + r[0], pp[1] + r[1], pp[2] + r[2]])
                }
                _ => (q, t),
            };
            out.push(pose);
        }
        out
    }
}

/// Hamilton product `a * b` of `[x, y, z, w]` quaternions.
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// Rotate `v` by the unit quaternion `q`.
pub fn quat_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let t = [2.0 * (y * v[2] - z * v[1]), 2.0 * (z * v[0] - x * v[2]), 2.0 * (x * v[1] - y * v[0])];
    [
        v[0] + w * t[0] + (y * t[2] - z * t[1]),
        v[1] + w * t[1] + (z * t[0] - x * t[2]),
        v[2] + w * t[2] + (x * t[1] - y * t[0]),
    ]
}

const MAGIC: &[u8] = b"animation\0";
const VERSION: &[u8] = b"00.00.00.0036";
const CHANNELS: usize = 9;
const BONE_RECORD: usize = 52;

struct Reader<'a> {
    d: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes(&self, o: usize, n: usize) -> Result<&'a [u8], AnimError> {
        match o.checked_add(n).and_then(|e| self.d.get(o..e)) {
            Some(b) => Ok(b),
            None => err(format!("truncated at 0x{o:X}+{n}")),
        }
    }
    fn u8(&self, o: usize) -> Result<u8, AnimError> {
        Ok(self.bytes(o, 1)?[0])
    }
    fn u16(&self, o: usize) -> Result<u16, AnimError> {
        Ok(u16::from_be_bytes(self.bytes(o, 2)?.try_into().unwrap()))
    }
    fn u32(&self, o: usize) -> Result<u32, AnimError> {
        Ok(u32::from_be_bytes(self.bytes(o, 4)?.try_into().unwrap()))
    }
    fn f32(&self, o: usize) -> Result<f32, AnimError> {
        Ok(f32::from_bits(self.u32(o)?))
    }
    fn f32x3(&self, o: usize) -> Result<[f32; 3], AnimError> {
        Ok([self.f32(o)?, self.f32(o + 4)?, self.f32(o + 8)?])
    }
    fn cstr(&self, o: usize) -> Result<String, AnimError> {
        let tail = self.d.get(o..).ok_or_else(|| AnimError(format!("string offset 0x{o:X} out of range")))?;
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
    }
}

/// LSB-first reader over big-endian 32-bit words (0x82E2A440).
struct BitReader<'a> {
    d: &'a [u8],
    word: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(d: &'a [u8], byte_addr: usize) -> Self {
        BitReader { d, word: byte_addr & !3, bit: ((byte_addr & 3) * 8) as u32 }
    }
    fn word_at(&self, o: usize) -> u64 {
        self.d.get(o..o + 4).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()) as u64)
    }
    fn read(&mut self, n: u32) -> Result<u32, AnimError> {
        if self.word + 4 > self.d.len() {
            return err("bitstream runs past the end of the asset");
        }
        let both = self.word_at(self.word) | (self.word_at(self.word + 4) << 32);
        let v = ((both >> self.bit) & ((1u64 << n) - 1)) as u32;
        self.bit += n;
        self.word += (self.bit as usize >> 5) * 4;
        self.bit &= 31;
        Ok(v)
    }
    fn position_bits(&self) -> usize {
        self.word * 8 + self.bit as usize
    }
}

#[derive(Debug, Clone, Copy)]
enum Chan {
    Default,
    Const(i32),
    Animated { base: i32, width: u32 },
}

/// Channel descriptor of a track group (see module docs).
struct Descriptor {
    scales: [f32; CHANNELS],
    defaults: [f32; CHANNELS],
    ctrl: usize,
    values: usize,
    widths: usize,
    values_len: usize,
    widths_len: usize,
    stride: usize,
    tracks: usize,
    frames: usize,
    wide: bool,
}

fn descriptor(r: &Reader, cd: usize) -> Result<Descriptor, AnimError> {
    let channels = r.u16(cd + 0x1E)? as usize;
    if channels != CHANNELS {
        return err(format!("{channels} channels per track (expected {CHANNELS})"));
    }
    let scales_at = r.u32(cd + 4)? as usize;
    let defaults_at = r.u32(cd + 8)? as usize;
    let mut scales = [0.0; CHANNELS];
    let mut defaults = [0.0; CHANNELS];
    for i in 0..CHANNELS {
        scales[i] = r.f32(scales_at + 4 * i)?;
        defaults[i] = r.f32(defaults_at + 4 * i)?;
    }
    Ok(Descriptor {
        scales,
        defaults,
        ctrl: r.u32(cd + 0xC)? as usize,
        values: r.u32(cd + 0x10)? as usize,
        widths: r.u32(cd + 0x14)? as usize,
        values_len: r.u16(cd + 0x18)? as usize,
        widths_len: r.u16(cd + 0x1A)? as usize,
        stride: r.u16(cd + 0x1C)? as usize,
        tracks: r.u16(cd + 0x22)? as usize,
        frames: r.u16(cd + 0x24)? as usize,
        wide: r.u8(cd + 0x26)? != 0,
    })
}

/// Walk the control bytes once: the per-track channel plan (constants, bases and widths resolved).
fn plan(r: &Reader, ds: &Descriptor) -> Result<Vec<[Chan; CHANNELS]>, AnimError> {
    let mut ctrl = ds.ctrl;
    let mut value = ds.values;
    let mut width_nibble = 0usize; // index into the width stream (nibbles, or bytes when wide)
    let mut out = Vec::with_capacity(ds.tracks);
    for _ in 0..ds.tracks {
        let mut ch = [Chan::Default; CHANNELS];
        let mut group = |byte: u8, first: usize| -> Result<(), AnimError> {
            for k in 0..3 {
                let present = 0x80u8 >> k;
                if byte & present == 0 {
                    continue;
                }
                let v = if ds.wide {
                    let v = r.u32(value)? as i32;
                    value += 4;
                    v
                } else {
                    let v = r.u16(value)? as i16 as i32;
                    value += 2;
                    v
                };
                ch[first + k] = if byte & (present >> 3) != 0 {
                    Chan::Const(v)
                } else {
                    let w = if ds.wide {
                        r.u8(ds.widths + width_nibble)? as u32
                    } else {
                        let b = r.u8(ds.widths + width_nibble / 2)?;
                        ((b >> ((width_nibble & 1) * 4)) & 0xF) as u32
                    };
                    width_nibble += 1;
                    Chan::Animated { base: v, width: (w + 1).min(32) }
                };
            }
            Ok(())
        };
        let b0 = r.u8(ctrl)?;
        group(b0, 0)?;
        match b0 & 3 {
            0 => {}
            1 => {
                ctrl += 1;
                group(r.u8(ctrl)?, 6)?;
            }
            2 => {
                ctrl += 1;
                group(r.u8(ctrl)?, 3)?;
            }
            _ => {
                ctrl += 1;
                group(r.u8(ctrl)?, 3)?;
                ctrl += 1;
                group(r.u8(ctrl)?, 6)?;
            }
        }
        ctrl += 1;
        out.push(ch);
    }
    // Sanity: the streams must fit their declared regions (4-byte padded in the files).
    let values_used = value - ds.values;
    let widths_used = if ds.wide { width_nibble } else { width_nibble.div_ceil(2) };
    if values_used > ds.values_len || widths_used > ds.widths_len {
        return err(format!(
            "control walk overran: values {values_used}/{} bytes, widths {widths_used}/{} bytes",
            ds.values_len, ds.widths_len
        ));
    }
    Ok(out)
}

/// Decode every frame: `[frame][track][channel]`.
fn decode_frames(d: &[u8], ds: &Descriptor, plan: &[[Chan; CHANNELS]]) -> Result<Vec<Vec<[f32; CHANNELS]>>, AnimError> {
    let first = ds.values + ds.values_len + ds.widths_len;
    let mut frames = Vec::with_capacity(ds.frames);
    for f in 0..ds.frames {
        let start = first + f * ds.stride;
        let mut br = BitReader::new(d, start);
        let mut tracks = Vec::with_capacity(plan.len());
        for chans in plan {
            let mut v = [0.0f32; CHANNELS];
            for (i, c) in chans.iter().enumerate() {
                v[i] = match *c {
                    Chan::Default => ds.defaults[i],
                    Chan::Const(k) => k as f32 * ds.scales[i],
                    Chan::Animated { base, width } => {
                        let bits = br.read(width)?;
                        ((bits as f64 + base as f64) as f32) * ds.scales[i]
                    }
                };
            }
            tracks.push(v);
        }
        let used = br.position_bits() - start * 8;
        if used > ds.stride * 8 {
            return err(format!("frame {f}: bitstream uses {used} bits, stride is {} bytes", ds.stride));
        }
        frames.push(tracks);
    }
    Ok(frames)
}

/// Locate the asset inside the CAFF container.
fn asset(file: &[u8]) -> Result<&[u8], AnimError> {
    if !file.starts_with(b"CAFF") {
        return err("not a CAFF container");
    }
    let r = Reader { d: file };
    let mut at = r.u32(0x48)? as usize;
    if file.get(at..at + MAGIC.len()) != Some(MAGIC) {
        at = file
            .windows(MAGIC.len())
            .take(0x1000)
            .position(|w| w == MAGIC)
            .ok_or_else(|| AnimError("no \"animation\" asset".into()))?;
    }
    let a = &file[at..];
    if a.get(10..10 + VERSION.len()) != Some(VERSION) {
        return err(format!("unsupported version {:?}", String::from_utf8_lossy(a.get(10..23).unwrap_or(&[]))));
    }
    Ok(a)
}

fn opt(v: u16) -> Option<u16> {
    (v != 0xFFFF).then_some(v)
}

fn skeleton(r: &Reader, at: usize, names_at: usize, count: usize) -> Result<Skeleton, AnimError> {
    let mut names = vec![String::new(); count];
    if names_at != 0 {
        let n = r.u32(names_at)? as usize;
        let entries = r.u32(names_at + 4)? as usize;
        for i in 0..n.min(4096) {
            let e = entries + 12 * i;
            let idx = r.u32(e + 8)? as usize;
            if idx < count {
                names[idx] = r.cstr(r.u32(e)? as usize)?;
            }
        }
    }
    let mut bones = Vec::with_capacity(count);
    for (i, name) in names.into_iter().enumerate() {
        let o = at + BONE_RECORD * i;
        let parent = opt(r.u16(o + 40)?);
        if let Some(p) = parent {
            if p as usize >= i {
                return err(format!("bone {i}: parent {p} does not precede it"));
            }
        }
        bones.push(Bone {
            name,
            parent,
            first_child: opt(r.u16(o + 42)?),
            next_sibling: opt(r.u16(o + 44)?),
            local: r.f32x3(o)?,
            world: r.f32x3(o + 12)?,
        });
    }
    Ok(Skeleton { bones })
}

/// Parse a whole `.anim.bin` (CAFF container included).
pub fn parse(file: &[u8]) -> Result<(Skeleton, Clip), String> {
    parse_inner(file).map_err(String::from)
}

fn parse_inner(file: &[u8]) -> Result<(Skeleton, Clip), AnimError> {
    let a = asset(file)?;
    let r = Reader { d: a };
    let duration = r.f32(0x1C)?;
    let bones = r.u16(0x20)? as usize;
    let frames = r.u16(0x22)? as usize;
    let quat_group = r.u32(0x24)? as usize;
    let skel_at = r.u32(0x2C)? as usize;
    let names_at = r.u32(0x34)? as usize;
    let second_group = r.u32(0x48)? as usize;

    let skel = skeleton(&r, skel_at, names_at, bones)?;

    if r.cstr(quat_group)? != "QUAT_BITSTREAM" {
        return err(format!("first track group is {:?}", r.cstr(quat_group)?));
    }
    if second_group != 0 && r.u32(second_group + 0x18)? != 0 {
        return err(format!("track group {:?} has data (not supported)", r.cstr(second_group)?));
    }
    let cd = r.u32(quat_group + 0x18)? as usize;
    if cd == 0 {
        return err("QUAT_BITSTREAM group has no channel descriptor");
    }
    let ds = descriptor(&r, cd)?;
    if ds.tracks != bones || ds.frames != frames {
        return err(format!("descriptor tracks/frames {}/{} vs header {bones}/{frames}", ds.tracks, ds.frames));
    }
    let plan = plan(&r, &ds)?;
    let raw = decode_frames(a, &ds, &plan)?;

    let mut rotations = Vec::with_capacity(frames);
    let mut translations = Vec::with_capacity(frames);
    let mut scales = Vec::with_capacity(frames);
    for frame in &raw {
        let mut rf = Vec::with_capacity(bones);
        let mut tf = Vec::with_capacity(bones);
        let mut sf = Vec::with_capacity(bones);
        for v in frame {
            let (x, y, z) = (v[0], v[1], v[2]);
            let s = 1.0 - (x * x + y * y + z * z);
            let w = if s > 0.0 { s.sqrt() } else { 0.0 };
            rf.push([x, y, z, w]);
            tf.push([v[3], v[4], v[5]]);
            sf.push([v[6], v[7], v[8]]);
        }
        rotations.push(rf);
        translations.push(tf);
        scales.push(sf);
    }
    let frame_time = if frames > 1 { duration / (frames - 1) as f32 } else { 1.0 / 30.0 };
    Ok((skel, Clip { frames: frames as u32, frame_time, duration, rotations, translations, scales }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_reader_is_lsb_first_in_be_words() {
        // Word 0 = 0x000000AB, word 1 = 0x00000001.
        let d = [0x00, 0x00, 0x00, 0xAB, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 0];
        let mut br = BitReader::new(&d, 0);
        assert_eq!(br.read(4).unwrap(), 0xB);
        assert_eq!(br.read(4).unwrap(), 0xA);
        // Straddle: bits 8..31 of word 0 are zero, bit 0 of word 1 is set.
        assert_eq!(br.read(25).unwrap(), 1 << 24);
        // Byte address 1 starts at bit 8 of word 0.
        let mut br = BitReader::new(&d, 1);
        assert_eq!(br.read(8).unwrap(), 0);
    }

    #[test]
    fn quaternion_helpers() {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let q = [0.0, h, 0.0, h]; // 90 degrees about +Y
        let v = quat_rotate(q, [1.0, 0.0, 0.0]);
        assert!((v[0]).abs() < 1e-6 && (v[2] + 1.0).abs() < 1e-6);
        let qq = quat_mul(q, q);
        assert!((qq[1] - 1.0).abs() < 1e-6);
    }
}
