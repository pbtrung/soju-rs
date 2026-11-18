//! Fixed, dependency-independent generators for reproducible property cases.
pub const SEEDS: [u64; 4] = [1, 0x5eed_2026, 0x9e37_79b9_7f4a_7c15, u64::MAX];

pub struct Generator(pub u64);
impl Generator {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn text(&mut self, length: usize) -> anyhow::Result<String> {
        const ALPHABET: [char; 16] = [
            'a', 'Z', '0', ' ', ';', '\\', '\r', '\n', 'é', '中', '🦀', ':', '=', '+', '/', 'ß',
        ];
        (0..length)
            .map(|_| {
                let index = usize::try_from(self.next() % 16)?;
                ALPHABET
                    .get(index)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("invalid generator index"))
            })
            .collect()
    }
}
