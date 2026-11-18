//! In-memory IRC reader benchmark, without sockets, TLS, storage or logging.
//! Run with: cargo bench --bench reader
//! Compare the same executable/workload before and after a reader change.

use std::hint::black_box;
use std::time::{Duration, Instant};

use soju::conn::Conn;
use soju::logger::Logger;
use tokio::io::AsyncWriteExt;

// Match the server binary's allocator.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

async fn sample(payload: usize, messages: usize) -> anyhow::Result<Duration> {
    let line = format!(
        ":nick!user@host PRIVMSG #channel :{}\r\n",
        "x".repeat(payload)
    );
    let chunk = line.repeat(128);
    let (a, mut b) = tokio::io::duplex(64 * 1024);
    let (conn, mut reader) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let producer = tokio::spawn(async move {
        for _ in 0..messages / 128 {
            b.write_all(chunk.as_bytes()).await?;
        }
        b.shutdown().await?;
        Ok::<(), std::io::Error>(())
    });
    let start = Instant::now();
    let mut received = 0;
    let mut bytes = 0;
    while let Some((message, permit)) = reader.read_message_limited().await? {
        bytes += black_box(&message).param(1).len();
        received += 1;
        drop(permit);
    }
    let elapsed = start.elapsed();
    producer.await??;
    conn.close();
    assert_eq!(received, messages);
    assert_eq!(bytes, messages * payload);
    Ok(elapsed)
}

fn memory_churn() -> anyhow::Result<()> {
    use soju::{
        database::Network,
        msg,
        msgstore::{MemoryStore, Store},
    };
    let mut store = Store::Memory(MemoryStore::new());
    let network = Network {
        id: 1,
        ..Network::new("benchmark")
    };
    for batch in 0..8 {
        for i in 0..100_000 {
            let target = format!("#{}", batch * 100_000 + i);
            store.append(
                &network,
                &target,
                &msg!("PRIVMSG", &target, "x".repeat(512)),
            )?;
        }
        if let Store::Memory(memory) = &store {
            let rss = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("VmRSS:"))
                        .map(str::to_owned)
                });
            println!("batch={batch} usage={:?} rss={rss:?}", memory.usage());
        }
    }
    for batch in 0..4 {
        for _ in 0..10_000 {
            store.append(
                &network,
                "#hot",
                &msg!("PRIVMSG", "#hot", "x".repeat(16 * 1024)),
            )?;
        }
        if let Store::Memory(memory) = &store {
            let rss = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("VmRSS:"))
                        .map(str::to_owned)
                });
            println!("hot_batch={batch} usage={:?} rss={rss:?}", memory.usage());
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--memory-churn") {
        return memory_churn();
    }
    if std::env::args().any(|arg| arg == "--layout") {
        use std::mem::size_of;
        for (name, size) in [
            ("Server", size_of::<soju::server::Server>()),
            ("User", size_of::<soju::user::User>()),
            ("Network", size_of::<soju::user::Network>()),
            ("Event", size_of::<soju::user::Event>()),
            ("UpstreamConn", size_of::<soju::upstream::UpstreamConn>()),
            (
                "DownstreamConn",
                size_of::<soju::downstream::DownstreamConn>(),
            ),
            ("Message", size_of::<soju::irc::Message>()),
            ("Metrics", size_of::<soju::metrics::Metrics>()),
        ] {
            println!("{name:20} {size:5} bytes");
        }
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        for (payload, messages) in [(32, 65536), (256, 65536), (4096, 8192)] {
            // Cargo also executes custom benchmarks in `test --all-targets`.
            // Verify each workload in debug builds; measure optimized builds.
            if cfg!(debug_assertions) {
                sample(payload, 128).await?;
                continue;
            }
            sample(payload, 1024).await?;
            let mut samples = [Duration::ZERO; 7];
            for slot in &mut samples {
                *slot = sample(payload, messages).await?;
            }
            samples.sort();
            println!(
                "payload={payload:4} bytes: median={:.1} ns/message, range={:.1}..{:.1}",
                samples[3].as_secs_f64() * 1e9 / f64::from(u32::try_from(messages)?),
                samples[0].as_secs_f64() * 1e9 / f64::from(u32::try_from(messages)?),
                samples[6].as_secs_f64() * 1e9 / f64::from(u32::try_from(messages)?),
            );
        }
        Ok(())
    })
}
