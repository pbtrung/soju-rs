//! Minimal BARE (https://baremessages.org) encoding, as used by upstream for
//! internal message IDs.

use anyhow::{Result, bail};

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn uint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    pub fn int(&mut self, v: i64) {
        self.uint(((v << 1) ^ (v >> 63)) as u64);
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
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b < 0x80 {
                return Ok(v);
            }
        }
        bail!("varint overflows a 64-bit integer")
    }

    pub fn int(&mut self) -> Result<i64> {
        let u = self.uint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    pub fn string(&mut self) -> Result<String> {
        let n = self.uint()? as usize;
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
}
