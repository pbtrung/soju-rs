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
        loop {
            let mut line = String::new();
            let res = tokio::time::timeout(IDENTD_TIMEOUT, async {
                // We only need to read lines with two port numbers
                let mut limited = (&mut r).take(512);
                limited.read_line(&mut line).await
            })
            .await;
            match res {
                Ok(Ok(n)) if n > 0 => {}
                _ => break,
            }
            let l = line.trim_end_matches(['\r', '\n']).to_owned();

            let reply = match parse_ident_query(&l) {
                None => {
                    let _ = r
                        .get_mut()
                        .write_all(format!("{l} : ERROR : INVALID-PORT\r\n").as_bytes())
                        .await;
                    break;
                }
                Some((local_port, remote_port)) => {
                    let k = IdentKey {
                        remote_host: remote_host.to_owned(),
                        remote_port,
                        local_port,
                    };
                    self.entries.read().unwrap().get(&k).cloned()
                }
            };
            let Some(ident) = reply else {
                let _ = r
                    .get_mut()
                    .write_all(format!("{l} : ERROR : NO-USER\r\n").as_bytes())
                    .await;
                break;
            };
            // The "OTHER" operating system may be rejected by IRC servers,
            // because it may be used when the ident string isn't stable. Use
            // "UNKNOWN" from RFC 1340 instead.
            if r.get_mut()
                .write_all(format!("{l} : USERID : UNKNOWN : {ident}\r\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
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
