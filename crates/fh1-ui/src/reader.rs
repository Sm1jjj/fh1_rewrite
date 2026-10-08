//! Bounds-checked big-endian cursor (every read returns `Result`).

use crate::{Error, Result};

pub(crate) struct Reader<'a> {
    pub d: &'a [u8],
    pub o: usize,
}

impl<'a> Reader<'a> {
    pub fn new(d: &'a [u8], o: usize) -> Self {
        Self { d, o }
    }

    pub fn remaining(&self) -> usize {
        self.d.len().saturating_sub(self.o)
    }

    pub fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        let end = self.o.checked_add(n).filter(|&e| e <= self.d.len());
        let end = end.ok_or(Error::Truncated { what, at: self.o })?;
        let s = &self.d[self.o..end];
        self.o = end;
        Ok(s)
    }

    pub fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        let mut a = [0; N];
        a.copy_from_slice(self.bytes(N, what)?);
        Ok(a)
    }

    pub fn u8(&mut self, what: &'static str) -> Result<u8> {
        Ok(self.array::<1>(what)?[0])
    }

    pub fn u16(&mut self, what: &'static str) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array(what)?))
    }

    pub fn u32(&mut self, what: &'static str) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array(what)?))
    }

    pub fn i32(&mut self, what: &'static str) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array(what)?))
    }

    pub fn f32(&mut self, what: &'static str) -> Result<f32> {
        Ok(f32::from_be_bytes(self.array(what)?))
    }

    pub fn u32s<const N: usize>(&mut self, what: &'static str) -> Result<[u32; N]> {
        let mut a = [0; N];
        for v in &mut a {
            *v = self.u32(what)?;
        }
        Ok(a)
    }

    pub fn f32s<const N: usize>(&mut self, what: &'static str) -> Result<[f32; N]> {
        let mut a = [0.0; N];
        for v in &mut a {
            *v = self.f32(what)?;
        }
        Ok(a)
    }

    /// `u32 length` + Latin-1 bytes (no terminator).
    pub fn lstr(&mut self, what: &'static str) -> Result<String> {
        let n = self.u32(what)? as usize;
        Ok(latin1(self.bytes(n, what)?))
    }
}

pub(crate) fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Big-endian u16 at `o`, if in range.
pub(crate) fn be_u16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

/// Big-endian u32 at `o`, if in range.
pub(crate) fn be_u32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

/// IEEE half → f32 (exact, incl. subnormals, ±0, inf, NaN).
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1F) as u32;
    let man = (h & 0x3FF) as u32;
    let bits = match (exp, man) {
        (0, 0) => sign,
        (0, m) => {
            // subnormal: m · 2^-24
            let v = m as f32 * (1.0 / 16_777_216.0);
            return if sign != 0 { -v } else { v };
        }
        (0x1F, m) => sign | 0x7F80_0000 | (m << 13),
        (e, m) => sign | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::f16_to_f32;

    #[test]
    fn half() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert!(f16_to_f32(0x8000).is_sign_negative());
        assert_eq!(f16_to_f32(0x7BFF), 65504.0);
    }
}
