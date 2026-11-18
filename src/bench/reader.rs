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

async fn sample(
    payload: usize,
    messages: usize,
    dispatch: Option<bool>,
) -> anyhow::Result<Duration> {
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
        let handle = move || {
            let bytes = black_box(&message).param(1).len();
            drop(permit);
            bytes
        };
        bytes += match dispatch {
            Some(true) => soju::server::blocking(handle).await?,
            Some(false)
                if tokio::runtime::Handle::current().runtime_flavor()
                    == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                tokio::task::block_in_place(handle)
            }
            _ => handle(),
        };
        received += 1;
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

fn receipt_clients() -> anyhow::Result<()> {
    use soju::{user::DeliveredStore, xirc::CASEMAPPING_ASCII};
    use std::collections::HashMap;
    for clients in [1, 8, 64, 512, 4096] {
        let mut store = DeliveredStore::new(CASEMAPPING_ASCII);
        let mut maps = Vec::new();
        for target in 0..16 {
            let mut names = HashMap::new();
            for client in 0..clients {
                let name = format!("client-{client:05}");
                store.store_id(&format!("#target{target}"), &name, "cursor")?;
                names.insert(name, "cursor");
            }
            maps.push(names);
        }
        let mut baseline = Vec::new();
        let mut current = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            let mut names: Vec<String> = Vec::new();
            for map in &maps {
                for name in map.keys() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            }
            baseline.push(start.elapsed());
            assert_eq!(black_box(names).len(), clients);
            let start = Instant::now();
            let names = store.clients();
            current.push(start.elapsed());
            assert_eq!(black_box(names).len(), clients);
        }
        baseline.sort_unstable();
        current.sort_unstable();
        println!(
            "receipt clients={clients} rows={} cache_accounting={:?} old_median={:?} set_median={:?}",
            clients * 16,
            store.usage(),
            baseline.get(3),
            current.get(3)
        );
    }
    Ok(())
}

async fn actor_boundary_samples() -> anyhow::Result<()> {
    let mut inline = Vec::new();
    let mut workers = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let mut state = 0u64;
        for _ in 0..2000 {
            state = black_box(state + 1);
        }
        inline.push(start.elapsed());
        assert_eq!(state, 2000);
        let start = Instant::now();
        let mut state = 0u64;
        for _ in 0..2000 {
            state = soju::server::blocking(move || black_box(state + 1)).await?;
        }
        workers.push(start.elapsed());
        assert_eq!(state, 2000);
    }
    inline.sort_unstable();
    workers.sort_unstable();
    println!(
        "actor events=2000 inline_median={:?} worker_median={:?}",
        inline.get(3),
        workers.get(3)
    );
    for payload in [32, 256] {
        let mut legacy = Vec::new();
        let mut owned = Vec::new();
        for _ in 0..3 {
            legacy.push(sample(payload, 4096, Some(false)).await?);
            owned.push(sample(payload, 4096, Some(true)).await?);
        }
        legacy.sort_unstable();
        owned.sort_unstable();
        println!(
            "IRC pipeline payload={payload} messages=4096 legacy_median={:?} worker_median={:?}",
            legacy.get(1),
            owned.get(1)
        );
    }
    Ok(())
}

async fn contention_sample(legacy: bool) -> anyhow::Result<Duration> {
    use soju::database::{Database, User};
    let path =
        std::env::temp_dir().join(format!("soju-actor-bench-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&path)?;
    let file = path.join("bench.db");
    let db = Database::open(
        file.to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?,
    )?;
    let lock = rusqlite::Connection::open(file)?;
    lock.execute_batch("BEGIN IMMEDIATE;")?;
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        lock.execute_batch("COMMIT;")
    });
    let start = Instant::now();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        start.elapsed()
    });
    tokio::task::yield_now().await;
    let operation = move || db.store_user(&mut User::new("contended"));
    if legacy {
        if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(operation)?;
        } else {
            operation()?;
        }
    } else {
        soju::server::blocking(operation).await??;
    }
    let latency = timer.await?;
    release
        .join()
        .map_err(|error| anyhow::anyhow!("writer thread panicked: {error:?}"))??;
    std::fs::remove_dir_all(path)?;
    Ok(latency)
}

fn actor_boundary() -> anyhow::Result<()> {
    for multithread in [false, true] {
        let mut runtime = if multithread {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = runtime.enable_all().build()?;
        runtime.block_on(async {
            println!("runtime_multithread={multithread}");
            actor_boundary_samples().await?;
            println!(
                "contention timer10ms legacy={:?} workers={:?}",
                contention_sample(true).await?,
                contention_sample(false).await?
            );
            Ok::<(), anyhow::Error>(())
        })?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--actor-boundary") {
        return actor_boundary();
    }
    if std::env::args().any(|arg| arg == "--receipt-clients") {
        return receipt_clients();
    }
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
                sample(payload, 128, None).await?;
                continue;
            }
            sample(payload, 1024, None).await?;
            let mut samples = [Duration::ZERO; 7];
            for slot in &mut samples {
                *slot = sample(payload, messages, None).await?;
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
