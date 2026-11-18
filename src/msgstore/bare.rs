//! Minimal BARE (<https://baremessages.org>) encoding, as used by upstream for
//! internal message IDs.

use anyhow::{Result, bail};

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn uint(&mut self, mut v: u64) {
        while v >= 0x80 {
            let [low, ..] = v.to_le_bytes();
            self.0.push(low | 0x80);
            v >>= 7;
        }
        let [low, ..] = v.to_le_bytes();
        self.0.push(low);
    }

    pub fn int(&mut self, v: i64) {
        self.uint(u64::from_ne_bytes(((v << 1) ^ (v >> 63)).to_ne_bytes()));
    }

    pub fn string(&mut self, s: &str) {
        self.uint(s.len() as u64);
        self.0.extend_from_slice(s.as_bytes());
    }
}

pub struct Reader<'a>(pub &'a [u8]);

impl Reader<'_> {
    pub fn uint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let Some((&b, rest)) = self.0.split_first() else {
                bail!("unexpected EOF");
            };
            self.0 = rest;
            if i == 9 && b > 1 {
                bail!("varint overflows a 64-bit integer");
            }
            v |= u64::from(b & 0x7f) << (7 * i);
            if b < 0x80 {
                return Ok(v);
            }
        }
        bail!("varint overflows a 64-bit integer")
    }

    pub fn int(&mut self) -> Result<i64> {
        let u = self.uint()?;
        Ok(i64::from_ne_bytes((u >> 1).to_ne_bytes()) ^ -i64::from(u & 1 != 0))
    }

    pub fn string(&mut self) -> Result<String> {
        let n = usize::try_from(self.uint()?)?;
        if n > self.0.len() {
            bail!("unexpected EOF");
        }
        let (s, rest) = self.0.split_at(n);
        self.0 = rest;
        // Go doesn't validate UTF-8: IDs it made may hold e.g. Latin-1
        // targets
        Ok(String::from_utf8_lossy(s).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn roundtrip() {
        let mut w = Writer::default();
        w.uint(0);
        w.uint(300);
        w.int(-1);
        w.int(i64::MIN);
        w.uint(u64::MAX);
        w.string("héllo");
        let mut r = Reader(&w.0);
        assert_eq!(r.uint().unwrap(), 0);
        assert_eq!(r.uint().unwrap(), 300);
        assert_eq!(r.int().unwrap(), -1);
        assert_eq!(r.int().unwrap(), i64::MIN);
        assert_eq!(r.uint().unwrap(), u64::MAX);
        assert_eq!(r.string().unwrap(), "héllo");
        assert!(r.uint().is_err());
    }
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn integer_boundaries_and_malformed_lengths() {
        for value in [i64::MIN, -1, 0, 1, i64::MAX] {
            let mut writer = Writer::default();
            writer.int(value);
            assert_eq!(Reader(&writer.0).int().unwrap(), value);
        }
        let mut length = Writer::default();
        length.uint(u64::MAX);
        assert!(Reader(&length.0).string().is_err());
        assert!(Reader(&[0xff; 10]).uint().is_err());
        assert!(Reader(&[0x80]).uint().is_err());
    }
}

#[cfg(test)]
mod properties {
    use super::{Reader, Writer};

    #[test]
    fn generated_integer_encodings_are_lossless_and_self_delimiting() -> anyhow::Result<()> {
        for seed in [1u64, 0x5eed_2026, u64::MAX] {
            let mut state = seed;
            for case in 0..4096 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let signed = i64::from_ne_bytes(state.to_ne_bytes());
                let mut writer = Writer::default();
                writer.uint(state);
                writer.int(signed);
                writer.string("é中🦀");
                let mut reader = Reader(&writer.0);
                assert_eq!(reader.uint()?, state, "seed={seed:x} case={case}");
                assert_eq!(reader.int()?, signed, "seed={seed:x} case={case}");
                assert_eq!(reader.string()?, "é中🦀");
                assert_eq!(reader.0, b"");
            }
        }
        Ok(())
    }
}
