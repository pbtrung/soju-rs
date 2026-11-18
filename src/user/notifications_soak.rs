//! Real-time saturation of the production queue/worker with slow local HTTP I/O.
use super::{Admission, Job, Queue, run_worker};
use crate::logger::Logger;
use crate::{database, msg};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const REQUEST: &[u8] = b"POST / HTTP/1.0\r\n\r\n";

fn snapshot(
    queue: &Queue,
    start: Instant,
    offered: u64,
    completed: u64,
    stage: &str,
) -> Result<()> {
    let pending = crate::state_lock::lock(&queue.pending);
    let tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let fds = std::fs::read_dir("/proc/self/fd")?.count();
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .context("missing RSS")?
        .trim();
    assert!(pending.jobs.len() <= 128 && pending.bytes <= 1024 * 1024);
    assert!(queue.metrics.active.load(Ordering::Relaxed) <= 1);
    println!(
        "SOAK push stage={stage} seconds={} offered={offered} completed={completed} pending={} bytes={} active={} overflow={} tasks={tasks} fds={fds} rss={rss}",
        start.elapsed().as_secs(),
        pending.jobs.len(),
        pending.bytes,
        queue.metrics.active.load(Ordering::Relaxed),
        queue.metrics.overflow_total.load(Ordering::Relaxed)
    );
    Ok(())
}

async fn endpoint(listener: tokio::net::TcpListener, stop: CancellationToken) -> Result<()> {
    loop {
        let (mut stream, _) = tokio::select! {
            () = stop.cancelled() => return Ok(()),
            result = listener.accept() => result?,
        };
        let mut request = vec![0; REQUEST.len()];
        if let Err(error) = stream.read_exact(&mut request).await {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                continue;
            }
            return Err(error.into());
        }
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
        stream
            .write_all(b"HTTP/1.0 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock resource soak; run scripts/resource-soak.py explicitly"]
async fn sustained_slow_push() -> Result<()> {
    let seconds =
        std::env::var("SOJU_SOAK_SECONDS").map_or(Ok(180), |value| value.parse::<u64>())?;
    anyhow::ensure!(
        (30..=3600).contains(&seconds),
        "SOJU_SOAK_SECONDS must be 30..3600"
    );
    let queue = Arc::new(Queue {
        pending: std::sync::Mutex::default(),
        wake: tokio::sync::Notify::new(),
        stop: CancellationToken::new(),
        metrics: Arc::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = CancellationToken::new();
    let responder = tokio::spawn(endpoint(listener, stop.clone()));
    let completed = Arc::new(AtomicU64::new(0));
    let completions = completed.clone();
    let worker = tokio::spawn(run_worker(queue.clone(), move |_| {
        let completions = completions.clone();
        async move {
            let request = async {
                let mut stream = tokio::net::TcpStream::connect(address).await?;
                stream.write_all(REQUEST).await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                Ok::<(), std::io::Error>(())
            };
            if tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .is_ok_and(|result| result.is_ok())
            {
                completions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }));
    let mut record = database::Network::new("soak");
    record.id = 1;
    let network = super::super::Network::new(&Logger::new(false), record, Vec::new());
    let start = Instant::now();
    let mut offered = 0;
    let mut next_sample = Duration::ZERO;
    let initial_fds = std::fs::read_dir("/proc/self/fd")?.count();
    let initial_tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    snapshot(&queue, start, offered, 0, "before")?;
    while start.elapsed() < Duration::from_secs(seconds) {
        for index in 0..512 {
            let target = format!("target-{offered}-{index}");
            let job = Job::new(
                &network,
                target.clone(),
                msg!("PRIVMSG", target, "notification"),
                None,
            );
            assert_ne!(queue.admit(job), Admission::Stopped);
            offered += 1;
        }
        if start.elapsed() >= next_sample {
            snapshot(
                &queue,
                start,
                offered,
                completed.load(Ordering::Relaxed),
                "load",
            )?;
            next_sample += Duration::from_secs(15);
            assert!(std::fs::read_dir("/proc/self/fd")?.count() <= initial_fds + 4);
            assert!(
                tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks()
                    <= initial_tasks
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    queue.stop.cancel();
    worker.await?;
    stop.cancel();
    responder.await??;
    tokio::time::sleep(Duration::from_secs(5)).await;
    snapshot(
        &queue,
        start,
        offered,
        completed.load(Ordering::Relaxed),
        "drained",
    )?;
    assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 0);
    assert_eq!(queue.metrics.active.load(Ordering::Relaxed), 0);
    assert!(completed.load(Ordering::Relaxed) > 0);
    assert!(std::fs::read_dir("/proc/self/fd")?.count() <= initial_fds);
    Ok(())
}
