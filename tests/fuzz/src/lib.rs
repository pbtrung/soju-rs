//! Bounded deterministic mutation fuzzing without instrumentation or native tooling.
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::path::{Path, PathBuf};

pub const MAX_INPUT: usize = 4096;

#[derive(Clone, Copy)]
pub enum Target {
    Irc,
    Proxy,
    Bare,
    Scfg,
}
impl Target {
    fn exercise(self, input: &[u8], runtime: &tokio::runtime::Runtime) {
        match self {
            Self::Irc => {
                let text = String::from_utf8_lossy(input);
                if let Ok(message) = soju::irc::Message::parse(&text) {
                    let canonical = message.to_string();
                    // The compatibility parser accepts malformed command tokens
                    // too. Only lexical IRC commands have a canonical roundtrip.
                    let valid_command = !message.command.is_empty()
                        && (message
                            .command
                            .bytes()
                            .all(|byte| byte.is_ascii_alphabetic())
                            || (message.command.len() == 3
                                && message.command.bytes().all(|byte| byte.is_ascii_digit())));
                    if valid_command
                        && !canonical.contains(['\r', '\n'])
                        && let Ok(parsed) = soju::irc::Message::parse(&canonical)
                    {
                        assert_eq!(parsed.to_string(), canonical);
                    }
                }
                let tags = soju::irc::Tags::parse(&text);
                let canonical = tags.to_string();
                assert_eq!(soju::irc::Tags::parse(&canonical).to_string(), canonical);
            }
            Self::Proxy => runtime.block_on(async {
                // A byte slice supplies EOF immediately, so malformed declared
                // lengths cannot hang waiting for a peer or grow input storage.
                drop(soju::proxyproto::read_header(input).await);
            }),
            Self::Bare => {
                drop(soju::msgstore::parse_msg_id(&URL_SAFE_NO_PAD.encode(input)));
                if let Ok(text) = std::str::from_utf8(input) {
                    drop(soju::msgstore::parse_msg_id(text));
                }
            }
            Self::Scfg => {
                drop(soju::scfg::parse(&String::from_utf8_lossy(input)));
            }
        }
    }
}

struct Generator(u64);
impl Generator {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn index(&mut self, length: usize) -> Result<usize> {
        Ok(usize::try_from(self.next() % u64::try_from(length)?)?)
    }
    fn mutate(&mut self, bytes: &mut Vec<u8>) -> Result<()> {
        let [value, ..] = self.next().to_le_bytes();
        let index = self.index(bytes.len() + 1)?;
        match self.next() % 4 {
            0 if bytes.len() < MAX_INPUT => bytes.insert(index, value),
            1 if index < bytes.len() => {
                bytes.remove(index);
            }
            2 => {
                if let Some(byte) = bytes.get_mut(index) {
                    *byte ^= value;
                }
            }
            _ => bytes.truncate(index),
        }
        Ok(())
    }
}

fn corpus(directory: &Path) -> Result<Vec<Vec<u8>>> {
    let mut paths: Vec<_> = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    paths.sort();
    let mut cases = vec![Vec::new()];
    for path in paths {
        if path.is_file() {
            let length = std::fs::metadata(&path)?.len();
            if length > u64::try_from(MAX_INPUT)? {
                bail!("oversized corpus input: {}", path.display());
            }
            cases.push(std::fs::read(path)?);
        }
    }
    Ok(cases)
}

fn minimize(target: Target, runtime: &tokio::runtime::Runtime, mut bytes: Vec<u8>) -> Vec<u8> {
    let mut chunk = bytes.len() / 2;
    let mut attempts = 0;
    while chunk > 0 && attempts < 128 {
        let mut offset = 0;
        while offset + chunk <= bytes.len() && attempts < 128 {
            let mut candidate = bytes.clone();
            candidate.drain(offset..offset + chunk);
            attempts += 1;
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                target.exercise(&candidate, runtime);
            }))
            .is_err()
            {
                bytes = candidate;
            } else {
                offset += chunk;
            }
        }
        chunk /= 2;
    }
    bytes
}

/// Runs a replay file or a fixed-seed mutation campaign with a strict size bound.
///
/// # Errors
/// Returns argument, corpus, runtime or filesystem errors, or a discovered crash.
pub fn run(target: Target, name: &str) -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let first = args.next();
    if first.as_deref() == Some("--replay") {
        let path = args.next().context("--replay requires a file")?;
        let bytes = std::fs::read(path)?;
        if bytes.len() > MAX_INPUT {
            bail!("oversized replay input");
        }
        target.exercise(&bytes, &runtime);
        return Ok(());
    }
    let iterations = first.map_or(Ok(10_000), |value| value.parse::<usize>())?;
    if iterations > 1_000_000 {
        bail!("iteration budget exceeds 1000000");
    }
    let seed = args
        .next()
        .map_or(Ok(0x5eed_2026_u64), |value| value.parse::<u64>())?;
    if seed == 0 {
        bail!("seed must be nonzero");
    }
    let cases = corpus(&root.join("corpus").join(name))?;
    let mut generator = Generator(seed);
    for iteration in 0..iterations + cases.len() {
        let mut input = cases
            .get(iteration % cases.len())
            .context("empty corpus")?
            .clone();
        if iteration >= cases.len() {
            for _ in 0..=(generator.next() % 8) {
                generator.mutate(&mut input)?;
            }
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            target.exercise(&input, &runtime);
        }))
        .is_err()
        {
            let minimized = minimize(target, &runtime, input);
            let path = root
                .join("artifacts")
                .join(format!("{name}-{seed}-{iteration}"));
            std::fs::create_dir_all(root.join("artifacts"))?;
            std::fs::write(&path, &minimized)?;
            bail!(
                "crash target={name} seed={seed} iteration={iteration} bytes={} artifact={}",
                minimized.len(),
                path.display()
            );
        }
    }
    println!(
        "PASS target={name} seed={seed} mutations={iterations} corpus={} max_input={MAX_INPUT}",
        cases.len()
    );
    Ok(())
}
