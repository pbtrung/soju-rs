//! In-memory IRC reader benchmark, without sockets, TLS, storage or logging.
//! Run with: cargo run --release --example bench-reader
//! Compare the same executable/workload before and after a reader change.

use std::hint::black_box;
use std::time::{Duration, Instant};

use soju::conn::Conn;
use soju::logger::Logger;
use tokio::io::AsyncWriteExt;

// Match the server binary's allocator.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

async fn sample(payload: usize, messages: usize) -> Duration {
    let line = format!(
        ":nick!user@host PRIVMSG #channel :{}\r\n",
        "x".repeat(payload)
    );
    let chunk = line.repeat(128);
    let (a, mut b) = tokio::io::duplex(64 * 1024);
    let (conn, mut reader) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let producer = tokio::spawn(async move {
        for _ in 0..messages / 128 {
            b.write_all(chunk.as_bytes()).await.unwrap();
        }
        b.shutdown().await.unwrap();
    });
    let start = Instant::now();
    let mut received = 0;
    let mut bytes = 0;
    while let Some((message, permit)) = reader.read_message_limited().await.unwrap() {
        bytes += black_box(&message).param(1).len();
        received += 1;
        drop(permit);
    }
    let elapsed = start.elapsed();
    producer.await.unwrap();
    conn.close();
    assert_eq!(received, messages);
    assert_eq!(bytes, messages * payload);
    elapsed
}

fn main() {
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
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        for (payload, messages) in [(32, 65536), (256, 65536), (4096, 8192)] {
            sample(payload, 1024).await;
            let mut samples = Vec::new();
            for _ in 0..7 {
                samples.push(sample(payload, messages).await);
            }
            samples.sort();
            println!(
                "payload={payload:4} bytes: median={:.1} ns/message, range={:.1}..{:.1}",
                samples[3].as_nanos() as f64 / messages as f64,
                samples[0].as_nanos() as f64 / messages as f64,
                samples[6].as_nanos() as f64 / messages as f64,
            );
        }
    });
}
