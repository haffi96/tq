/// Removes emulation prevention bytes (`00 00 03` -> `00 00`) from a NAL
/// unit payload, giving the RBSP.
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader over an RBSP with Exp-Golomb support.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos >> 3)?;
        let v = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    /// Reads `n` (at most 32) bits.
    pub fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v as u32)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Unsigned Exp-Golomb code.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u64;
        Some(((1u64 << zeros) - 1 + rest) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_emulation_prevention() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
        assert_eq!(unescape_rbsp(&[1, 0, 3, 0]), vec![1, 0, 3, 0]);
    }

    #[test]
    fn reads_exp_golomb() {
        // 1 | 010 | 011 | 00100 -> 0, 1, 2, 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
    }
}
