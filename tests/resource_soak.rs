//! Explicit wall-clock resource soak. Run through scripts/resource-soak.py.
use anyhow::{Context, Result};
use soju::database::{Database, Network};
use soju::listen::{parse_listen_uri, spawn_listener};
use soju::logger::Logger;
use soju::msgstore::{MemoryStore, Store};
use soju::server::{Server, ServerConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn duration() -> Result<Duration> {
    let seconds =
        std::env::var("SOJU_SOAK_SECONDS").map_or(Ok(180), |value| value.parse::<u64>())?;
    anyhow::ensure!(
        (30..=3600).contains(&seconds),
        "SOJU_SOAK_SECONDS must be 30..3600"
    );
    Ok(Duration::from_secs(seconds))
}
fn resources() -> Result<(usize, usize)> {
    let descriptors = std::fs::read_dir("/proc/self/fd")?.count();
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .context("missing RSS")?
        .split_whitespace()
        .next()
        .context("missing RSS value")?
        .parse()?;
    Ok((descriptors, rss))
}
async fn sessions(server: &Server, expected: usize) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.accepted_sessions() != expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    Ok(())
}
async fn connection_burst(server: &Arc<Server>, address: std::net::SocketAddr) -> Result<()> {
    let mut clients = Vec::new();
    for _ in 0..16 {
        let mut stream = TcpStream::connect(address).await?;
        stream.write_all(b"CAP LS 302\r\n").await?;
        let mut client = BufReader::new(stream);
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_line(&mut reply)).await??;
        assert_eq!(soju::irc::Message::parse(&reply)?.command, "CAP");
        clients.push(client);
    }
    sessions(server, 16).await?;
    for _ in 0..4 {
        let mut rejected = TcpStream::connect(address).await?;
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rejected.read(&mut byte)).await??,
            0
        );
    }
    drop(clients);
    sessions(server, 0).await
}
fn append_senders(store: &mut Store, network: &Network, generation: u64) -> Result<()> {
    for index in 0..2000 {
        let target = format!("sender-{generation}-{index}");
        store.append(
            network,
            &target,
            &soju::msg!("PRIVMSG", "me", "x".repeat(512)),
        )?;
    }
    for _ in 0..256 {
        store.append(
            network,
            "hot",
            &soju::msg!("PRIVMSG", "me", "x".repeat(16 * 1024)),
        )?;
    }
    Ok(())
}
fn snapshot(
    stage: &str,
    start: Instant,
    server: &Server,
    store: &Store,
    generation: u64,
) -> Result<()> {
    let (fds, rss) = resources()?;
    let tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let Store::Memory(memory) = store else {
        anyhow::bail!("expected memory history")
    };
    let usage = memory.usage();
    assert!(usage.targets <= 1024 && usage.messages <= 16_384 && usage.bytes <= 16 * 1024 * 1024);
    assert!(server.accepted_sessions() <= 16);
    println!(
        "SOAK history stage={stage} seconds={} batches={generation} tasks={tasks} fds={fds} rss_kib={rss} sessions={} targets={} messages={} bytes={}",
        start.elapsed().as_secs(),
        server.accepted_sessions(),
        usage.targets,
        usage.messages,
        usage.bytes
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock resource soak; run scripts/resource-soak.py explicitly"]
async fn sustained_connections_and_senders() -> Result<()> {
    let duration = duration()?;
    let server = Server::new(Database::open_in_memory()?, Logger::new(false));
    let mut config = ServerConfig::default();
    config.basic.max_accepted_connections = 16;
    server.set_config(config);
    let address = spawn_listener(
        server.clone(),
        parse_listen_uri("irc+insecure://127.0.0.1:0")?,
        None,
    )
    .await?
    .context("missing bound address")?;
    let mut store = Store::Memory(MemoryStore::new());
    let network = Network {
        id: 1,
        ..Network::new("soak")
    };
    let start = Instant::now();
    snapshot("before", start, &server, &store, 0)?;
    let (initial_fds, _) = resources()?;
    let initial_tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let mut generation = 0;
    let mut next_sample = Duration::ZERO;
    while start.elapsed() < duration {
        connection_burst(&server, address).await?;
        append_senders(&mut store, &network, generation)?;
        generation += 1;
        if start.elapsed() >= next_sample {
            snapshot("load", start, &server, &store, generation)?;
            next_sample += Duration::from_secs(15);
            assert!(
                resources()?.0 <= initial_fds + 8,
                "descriptors retained after disconnected burst"
            );
            assert!(
                tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks()
                    <= initial_tasks + 4,
                "connection tasks retained after burst"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    snapshot("loaded", start, &server, &store, generation)?;
    store.delete_network(network.id);
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    sessions(&server, 0).await?;
    snapshot("drained", start, &server, &store, generation)?;
    assert!(resources()?.0 <= initial_fds);
    assert!(
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
            <= initial_tasks
    );
    Ok(())
}
