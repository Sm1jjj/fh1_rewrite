//! Hashes used by the UI data.
//!
//! - **ahash** (Anark Gameface): `h = (h + c) * 65599 mod 2^32` over the bytes, case sensitive.
//!   VERIFIED: reproduces property, slide, event and object names (see `docs/UI.md`).
//!   Object names use all 32 bits, event / slide-name arguments 31 bits, property keys 27 bits
//!   with a type code in the top 5 bits.
//! - **str id hash** (`.str` string tables and gamedb `_&n`): `h = 0xFFFF; h ^= c; h = rotl16(h, 7)`
//!   over the lower-cased Latin-1 bytes. VERIFIED on all 223,625 id names of the 22 languages.

/// Full 32-bit Anark hash (object / record names).
pub const fn ahash(s: &[u8]) -> u32 {
    let mut h: u32 = 0;
    let mut i = 0;
    while i < s.len() {
        h = h.wrapping_add(s[i] as u32).wrapping_mul(65599);
        i += 1;
    }
    h
}

/// 31-bit form stored for event names and GOTO_SLIDE slide-name arguments (`SHOWN` → 0x519E02AF).
pub const fn ahash31(s: &[u8]) -> u32 {
    ahash(s) & 0x7FFF_FFFF
}

/// 27-bit form = a property key without its type bits (track keys carry exactly this).
pub const fn ahash27(s: &[u8]) -> u32 {
    ahash(s) & MASK27
}

pub const MASK27: u32 = (1 << 27) - 1;

/// Property key type codes (`key >> 27 & 15`). VERIFIED per name over all scenes.
pub const TYPE_INT: u32 = 1;
pub const TYPE_FLOAT: u32 = 3;
pub const TYPE_BOOL: u32 = 4;
pub const TYPE_STRING: u32 = 5;
/// Added to the type for dynamic/custom attributes (behaviour attributes, SET_PROPERTY keys).
/// VERIFIED as a flag; exact meaning GUESS.
pub const TYPE_DYNAMIC: u32 = 16;

/// Property key = `(type << 27) | ahash27(name)`.
pub const fn prop_key(ty: u32, name: &[u8]) -> u32 {
    (ty << 27) | ahash27(name)
}

/// `.str` id / file-stem hash on bytes that are already lower case.
pub const fn str_hash_lower(s: &[u8]) -> u16 {
    let mut h: u16 = 0xFFFF;
    let mut i = 0;
    while i < s.len() {
        h ^= s[i] as u16;
        h = h.rotate_left(7);
        i += 1;
    }
    h
}

/// `.str` id / file-stem hash (case insensitive). Characters are lower-cased then taken as
/// Latin-1; anything outside Latin-1 contributes its low byte (never happens on the disc).
pub fn str_hash(name: &str) -> u16 {
    let mut h: u16 = 0xFFFF;
    for c in name.chars().flat_map(char::to_lowercase) {
        h ^= (c as u32 & 0xFF) as u16;
        h = h.rotate_left(7);
    }
    h
}

/// gamedb `_&n` value for `File:IDS_x`: `n = H(file) << 16 | H(id)`.
pub fn gamedb_ref(file: &str, id: &str) -> u32 {
    (u32::from(str_hash(file)) << 16) | u32::from(str_hash(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anark_hash() {
        assert_eq!(ahash(b"SHOWN"), 0xD19E02AF);
        assert_eq!(ahash31(b"SHOWN"), 0x519E02AF);
        assert_eq!(ahash(b"opacity"), 0x6191C315);
        assert_eq!(ahash27(b"position.x"), 0x027C230D);
        assert_eq!(ahash(b"Master Slide"), 0xB9C8F82D);
        assert_eq!(ahash(b"EventFire"), 0x633F34B0);
        assert_eq!(prop_key(TYPE_STRING, b"textstring"), (5 << 27) | 0x0112_4062);
    }

    #[test]
    fn string_hash() {
        assert_eq!(str_hash("Data_Car"), 0xCD49);
        assert_eq!(str_hash("data_car"), str_hash_lower(b"data_car"));
        assert_eq!(gamedb_ref("Data_Car", "IDS_DisplayName_1032"), 3_444_170_713);
        assert_eq!(gamedb_ref("Data_Car", "IDS_DisplayName_249"), 3_444_160_236);
    }
}
