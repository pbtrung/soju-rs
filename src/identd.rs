//! An Identification Protocol server, as defined in RFC 1413.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::database::split_host_port;

const IDENTD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct IdentKey {
    remote_host: String,
    remote_port: u16,
    local_port: u16,
}

fn split(addr: &str) -> Option<(String, u16)> {
    let (h, p) = split_host_port(addr).ok()?;
    Some((h, p.parse().ok()?))
}

fn new_ident_key(remote_addr: &str, local_addr: &str) -> Option<IdentKey> {
    let (remote_host, remote_port) = split(remote_addr)?;
    let (_, local_port) = split(local_addr)?;
    Some(IdentKey {
        remote_host,
        remote_port,
        local_port,
    })
}

/// An ident server.
#[derive(Default)]
pub struct Identd {
    entries: RwLock<HashMap<IdentKey, String>>,
}

impl Identd {
    pub fn new() -> Identd {
        Identd::default()
    }

    pub fn store(&self, remote_addr: &str, local_addr: &str, ident: &str) {
        if let Some(k) = new_ident_key(remote_addr, local_addr) {
            self.entries.write().unwrap().insert(k, ident.to_owned());
        }
    }

    pub fn delete(&self, remote_addr: &str, local_addr: &str) {
        if let Some(k) = new_ident_key(remote_addr, local_addr) {
            self.entries.write().unwrap().remove(&k);
        }
    }

    /// Handles one ident client connection. remote_host is the client's IP.
    pub async fn handle<S: AsyncRead + AsyncWrite + Unpin>(&self, stream: S, remote_host: &str) {
        let mut r = BufReader::new(stream);
        while let Ok(Ok(true)) =
            tokio::time::timeout(IDENTD_TIMEOUT, self.handle_query(&mut r, remote_host)).await
        {
        }
    }

    async fn handle_query<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        r: &mut BufReader<S>,
        remote_host: &str,
    ) -> std::io::Result<bool> {
        let mut line = String::new();
        // Bound both memory use and the time spent reading AND writing a query.
        (&mut *r).take(512).read_line(&mut line).await?;
        if !line.ends_with('\n') {
            return Ok(false);
        }
        let query = line.trim_end_matches(['\r', '\n']);
        let (reply, keep_open) = match parse_ident_query(query) {
            None => (format!("{query} : ERROR : INVALID-PORT\r\n"), false),
            Some((local_port, remote_port)) => {
                let key = IdentKey {
                    remote_host: remote_host.to_owned(),
                    remote_port,
                    local_port,
                };
                let ident = self.entries.read().unwrap().get(&key).cloned();
                match ident {
                    // UNKNOWN is from RFC 1340; IRC servers may reject OTHER.
                    Some(ident) => (format!("{query} : USERID : UNKNOWN : {ident}\r\n"), true),
                    None => (format!("{query} : ERROR : NO-USER\r\n"), false),
                }
            }
        };
        r.get_mut().write_all(reply.as_bytes()).await?;
        Ok(keep_open)
    }
}

fn parse_ident_query(l: &str) -> Option<(u16, u16)> {
    let (local, remote) = l.split_once(',')?;
    let local: i64 = local.trim().parse().ok()?;
    let remote: i64 = remote.trim().parse().ok()?;
    if local <= 0 || remote <= 0 || local > 65535 || remote > 65535 {
        return None;
    }
    Some((local as u16, remote as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stalled_reply_times_out() {
        let identd = Identd::new();
        identd.store("10.0.0.1:2", "10.0.0.2:1", "user");
        let (a, mut b) = tokio::io::duplex(16);
        let task = tokio::spawn(async move { identd.handle(a, "10.0.0.1").await });
        b.write_all(b"1,2\r\n").await.unwrap();
        // The complete reply exceeds the pipe capacity. The peer never reads.
        tokio::time::timeout(IDENTD_TIMEOUT + Duration::from_secs(1), task)
            .await
            .expect("stalled ident writer was retained indefinitely")
            .unwrap();
    }

    #[tokio::test]
    async fn truncated_query_is_not_processed() {
        let identd = Identd::new();
        identd.store("10.0.0.1:2", "10.0.0.2:1", "user");
        let (a, mut b) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move { identd.handle(a, "10.0.0.1").await });
        // A syntactically valid prefix at the size cap is still not a line.
        let mut query = b"1,2".to_vec();
        query.resize(512, b' ');
        query.extend_from_slice(b"5,6\r\n");
        b.write_all(&query).await.unwrap();
        let mut reply = Vec::new();
        b.read_to_end(&mut reply).await.unwrap();
        assert!(reply.is_empty());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn query() {
        let identd = Identd::new();
        identd.store("10.0.0.1:6667", "10.0.0.2:40000", "abcdef");
        let (a, mut b) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            identd.handle(a, "10.0.0.1").await;
        });
        b.write_all(b"40000 , 6667\r\n").await.unwrap();
        let mut r = BufReader::new(&mut b);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "40000 , 6667 : USERID : UNKNOWN : abcdef\r\n");
        r.get_mut().write_all(b"1, 2\r\n").await.unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "1, 2 : ERROR : NO-USER\r\n");
        task.await.unwrap();
        assert_eq!(parse_ident_query("0,1"), None);
        assert_eq!(parse_ident_query("x"), None);
    }
}
