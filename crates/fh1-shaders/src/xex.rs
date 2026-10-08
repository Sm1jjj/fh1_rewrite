//! XEX2 executables (`default.xex`): decrypt and decompress the PE image, so shader containers
//! embedded in it can be extracted from the user's own disc (see docs/SHADERS.md).
//!
//! Layout (big-endian): `"XEX2", module_flags, pe_data_offset, reserved, security_info_offset,
//! optional_header_count`, then `(u32 key, u32 value-or-offset)` pairs. Key 0x3FF = file format
//! info `{u32 size, u16 encryption (0 none, 1 AES), u16 compression (1 basic, 2 normal/LZX)}`.
//! Security info: `+4 image_size`, `+0x110 load_address`, `+0x150` the file key (AES-128-ECB
//! encrypted with the retail key). Data at `pe_data_offset` is AES-128-CBC (IV 0) with that key.
//! Normal compression: blocks `{u32 next_block_size, [u8; 20] next_hash, (u16 chunk_size, data)*,
//! u16 0}` (the first block's size is in the format info); each chunk is one LZX frame.
//!
//! The Xbox 360 retail XEX key is not part of this repository. Encrypted images need it from the user:
//! `FH1_XEX_KEY=<32 hex digits>` or a file `data/xex_key.txt` holding the same (see README.md).

use crate::Error;

/// Where [`retail_key`] looks for the key file when `FH1_XEX_KEY` is not set.
pub const KEY_FILE: &str = "data/xex_key.txt";

/// The retail XEX key from `FH1_XEX_KEY` or [`KEY_FILE`] (32 hex digits; spaces, commas and `0x` are ignored).
pub fn retail_key() -> Result<[u8; 16], Error> {
    let text = match std::env::var("FH1_XEX_KEY") {
        Ok(v) => v,
        Err(_) => std::fs::read_to_string(KEY_FILE).map_err(|_| {
            Error::Unsupported(format!("default.xex is encrypted: put the Xbox 360 retail XEX key (32 hex digits) in {KEY_FILE} or FH1_XEX_KEY"))
        })?,
    };
    parse_key(&text).ok_or_else(|| Error::Unsupported("xex key: expected 32 hex digits".into()))
}

fn parse_key(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.replace("0x", "").replace("0X", "").chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

pub struct XexImage {
    pub load_address: u32,
    /// The PE image as mapped in memory (offset = virtual address - load_address).
    pub image: Vec<u8>,
}

fn be32(d: &[u8], o: usize) -> Result<u32, Error> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("xex"))
}

pub fn load(d: &[u8]) -> Result<XexImage, Error> {
    if d.get(..4) != Some(b"XEX2") {
        return Err(Error::BadMagic("XEX2"));
    }
    let pe_off = be32(d, 8)? as usize;
    let sec = be32(d, 0x10)? as usize;
    let n_headers = be32(d, 0x14)? as usize;
    let mut format_info = None;
    for i in 0..n_headers {
        let o = 0x18 + i * 8;
        if be32(d, o)? == 0x3FF {
            format_info = Some(be32(d, o + 4)? as usize);
        }
    }
    let fi = format_info.ok_or(Error::BadMagic("xex file format info"))?;
    let enc = u16::from_be_bytes([d[fi + 4], d[fi + 5]]);
    let comp = u16::from_be_bytes([d[fi + 6], d[fi + 7]]);
    let image_size = be32(d, sec + 4)? as usize;
    let load_address = be32(d, sec + 0x110)?;

    let mut data = d.get(pe_off..).ok_or(Error::Truncated("xex data"))?.to_vec();
    if enc == 1 {
        let mut key = [0u8; 16];
        key.copy_from_slice(d.get(sec + 0x150..sec + 0x160).ok_or(Error::Truncated("xex key"))?);
        let retail = Aes128::new(&retail_key()?);
        retail.decrypt_block(&mut key);
        let aes = Aes128::new(&key);
        let mut iv = [0u8; 16];
        for chunk in data.chunks_exact_mut(16) {
            let c: [u8; 16] = chunk.try_into().unwrap();
            let mut b = c;
            aes.decrypt_block(&mut b);
            for k in 0..16 {
                chunk[k] = b[k] ^ iv[k];
            }
            iv = c;
        }
    }

    let image = match comp {
        1 => {
            // Basic: (data_size, zero_size) pairs.
            let n = (be32(d, fi)? as usize - 8) / 8;
            let mut out = Vec::with_capacity(image_size);
            let mut p = 0;
            for i in 0..n {
                let ds = be32(d, fi + 8 + i * 8)? as usize;
                let zs = be32(d, fi + 12 + i * 8)? as usize;
                out.extend_from_slice(data.get(p..p + ds).ok_or(Error::Truncated("xex basic block"))?);
                out.resize(out.len() + zs, 0);
                p += ds;
            }
            out
        }
        2 => {
            let window = be32(d, fi + 8)?;
            let mut block_size = be32(d, fi + 12)? as usize;
            let window = match window {
                0x8000 => lzxd::WindowSize::KB32,
                0x10000 => lzxd::WindowSize::KB64,
                0x20000 => lzxd::WindowSize::KB128,
                0x40000 => lzxd::WindowSize::KB256,
                0x80000 => lzxd::WindowSize::KB512,
                0x100000 => lzxd::WindowSize::MB1,
                0x200000 => lzxd::WindowSize::MB2,
                _ => return Err(Error::Unsupported(format!("xex lzx window {window:#x}"))),
            };
            let mut lzx = lzxd::Lzxd::new(window);
            let mut out = Vec::with_capacity(image_size);
            let mut p = 0usize;
            while block_size != 0 && p < data.len() {
                let next = be32(&data, p)? as usize;
                let mut q = p + 24;
                loop {
                    let cs = u16::from_be_bytes([*data.get(q).ok_or(Error::Truncated("xex chunk"))?, *data.get(q + 1).ok_or(Error::Truncated("xex chunk"))?]) as usize;
                    q += 2;
                    if cs == 0 {
                        break;
                    }
                    let chunk = data.get(q..q + cs).ok_or(Error::Truncated("xex chunk data"))?;
                    let want = (image_size - out.len()).min(0x8000);
                    let frame = lzx.decompress_next(chunk, want).map_err(|e| Error::Lzx(format!("{e:?}")))?;
                    out.extend_from_slice(frame);
                    q += cs;
                }
                p += block_size;
                block_size = next;
            }
            out
        }
        0 => data,
        c => return Err(Error::Unsupported(format!("xex compression {c}"))),
    };
    if image.get(..2) != Some(b"MZ") {
        return Err(Error::BadMagic(if enc == 1 { "PE image (wrong XEX key?)" } else { "PE image" }));
    }
    Ok(XexImage { load_address, image })
}

/// Minimal AES-128 (decryption only), FIPS-197.
struct Aes128 {
    rk: [[u8; 16]; 11],
}

const SBOX: [u8; 256] = {
    // Generated from the AES affine transform over GF(2^8) inverses.
    let mut s = [0u8; 256];
    let mut p: u8 = 1;
    let mut q: u8 = 1;
    loop {
        // p *= 3
        p = p ^ (p << 1) ^ (if p & 0x80 != 0 { 0x1B } else { 0 });
        // q /= 3
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
        s[p as usize] = x ^ 0x63;
        if p == 1 {
            break;
        }
    }
    s[0] = 0x63;
    s
};

const INV_SBOX: [u8; 256] = {
    let mut inv = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        inv[SBOX[i] as usize] = i as u8;
        i += 1;
    }
    inv
};

fn xtime(x: u8) -> u8 {
    (x << 1) ^ if x & 0x80 != 0 { 0x1B } else { 0 }
}

fn mul(a: u8, b: u8) -> u8 {
    let (mut a, mut b, mut r) = (a, b, 0u8);
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    r
}

impl Aes128 {
    fn new(key: &[u8; 16]) -> Self {
        let mut rk = [[0u8; 16]; 11];
        rk[0] = *key;
        let mut rcon = 1u8;
        for r in 1..11 {
            let prev = rk[r - 1];
            let mut t = [prev[13], prev[14], prev[15], prev[12]];
            for x in &mut t {
                *x = SBOX[*x as usize];
            }
            t[0] ^= rcon;
            rcon = xtime(rcon);
            let mut k = [0u8; 16];
            for i in 0..4 {
                k[i] = prev[i] ^ t[i];
            }
            for i in 4..16 {
                k[i] = prev[i] ^ k[i - 4];
            }
            rk[r] = k;
        }
        Self { rk }
    }

    fn decrypt_block(&self, b: &mut [u8; 16]) {
        let add = |b: &mut [u8; 16], k: &[u8; 16]| {
            for i in 0..16 {
                b[i] ^= k[i];
            }
        };
        add(b, &self.rk[10]);
        for round in (0..10).rev() {
            // Inverse shift rows (column-major state).
            let s = *b;
            for c in 0..4 {
                for r in 0..4 {
                    b[c * 4 + r] = s[((c + 4 - r) % 4) * 4 + r];
                }
            }
            for x in b.iter_mut() {
                *x = INV_SBOX[*x as usize];
            }
            add(b, &self.rk[round]);
            if round > 0 {
                for c in 0..4 {
                    let col = [b[c * 4], b[c * 4 + 1], b[c * 4 + 2], b[c * 4 + 3]];
                    for r in 0..4 {
                        b[c * 4 + r] = mul(col[r], 14) ^ mul(col[(r + 1) % 4], 11) ^ mul(col[(r + 2) % 4], 13) ^ mul(col[(r + 3) % 4], 9);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes128_fips197_vector() {
        // FIPS-197 appendix C.1.
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let mut ct = [0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a];
        Aes128::new(&key).decrypt_block(&mut ct);
        let pt: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        assert_eq!(ct, pt);
    }

    #[test]
    fn key_text() {
        let k: [u8; 16] = core::array::from_fn(|i| i as u8 * 0x11);
        assert_eq!(parse_key("00112233445566778899aabbccddeeff\r\n"), Some(k));
        assert_eq!(parse_key("0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF"), Some(k));
        assert_eq!(parse_key("0011"), None);
    }
}

impl XexImage {
    /// Every embedded shader container: (virtual address, container bytes). The address is the
    /// stable name used in docs/SHADERS.md (e.g. 0x821b5928 = final post combine).
    pub fn shader_containers(&self) -> Vec<(u32, Vec<u8>)> {
        use crate::container::{ShaderBlob, MAGIC_PS, MAGIC_VS};
        let d = &self.image;
        let mut out = Vec::new();
        let mut o = 0;
        while o + 4 <= d.len() {
            let w = u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
            if (w == MAGIC_VS || w == MAGIC_PS) && ShaderBlob::parse(&d[o..], o).is_ok() {
                let len = ShaderBlob::container_len(&d[o..]).unwrap_or(4).max(4);
                out.push((self.load_address + o as u32, d[o..(o + len).min(d.len())].to_vec()));
                o += len & !3;
                continue;
            }
            o += 4;
        }
        out
    }
}

impl XexImage {
    /// FXL effect bodies embedded in the image (the post-processing / shadow-mask effect):
    /// (marker virtual address, bytes from the marker to the end of everything the effect uses).
    /// Parse them with `Effect::parse_embedded`.
    pub fn embedded_effects(&self) -> Vec<(u32, Vec<u8>)> {
        let d = &self.image;
        let mut out = Vec::new();
        let mut o = 0;
        while o + 4 <= d.len() {
            if d[o..o + 4] == [0xA3, 0xD7, 0x01, 0x41] {
                if let Ok((fx, used)) = crate::effect::Effect::parse_embedded(&d[o..]) {
                    if !fx.techniques.is_empty() {
                        out.push((self.load_address + o as u32, d[o..o + used].to_vec()));
                    }
                }
            }
            o += 4;
        }
        out
    }
}
