//! PROXY protocol (v1 and v2) parsing, see
//! https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

const V1_SIGNATURE: &[u8] = b"PROXY ";
const V2_SIGNATURE: &[u8] = b"\r\n\r\n\0\r\nQUIT\n";
const V1_MAX_LEN: usize = 107;
const V2_MAX_LEN: usize = 4096;

const PP2_TYPE_SSL: u8 = 0x20;
const PP2_SUBTYPE_SSL_CLIENT_CERT: u8 = 0x28;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// Source address, None for LOCAL/UNKNOWN connections
    pub source: Option<SocketAddr>,
    pub destination: Option<SocketAddr>,
    /// DER client certificate from a PP2_TYPE_SSL TLV
    pub client_cert: Option<Vec<u8>>,
}

/// A stream with bytes already read from it put back in front.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Prefixed<S> {
        Prefixed {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let pos = self.pos;
            buf.put_slice(&self.prefix[pos..pos + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Reads an optional PROXY header from the start of a stream. Bytes are
/// consumed one at a time only while they match a PROXY signature, so this
/// never blocks waiting for data a regular client wouldn't send. The
/// returned stream replays any consumed bytes which weren't part of a
/// header.
pub async fn read_header<S: AsyncRead + Unpin>(
    mut stream: S,
) -> Result<(Option<Header>, Prefixed<S>)> {
    let mut buf = Vec::new();
    loop {
        let i = buf.len();
        let v1 = i < V1_SIGNATURE.len() && buf[..] == V1_SIGNATURE[..i];
        let v2 = i < V2_SIGNATURE.len() && buf[..] == V2_SIGNATURE[..i];
        if !v1 && !v2 {
            break;
        }
        let mut b = [0u8; 1];
        if stream.read(&mut b).await? == 0 {
            // EOF: let the caller deal with it
            return Ok((None, Prefixed::new(buf, stream)));
        }
        buf.push(b[0]);
        if buf == V1_SIGNATURE {
            let header = read_v1(&mut stream).await?;
            return Ok((Some(header), Prefixed::new(Vec::new(), stream)));
        }
        if buf == V2_SIGNATURE {
            let header = read_v2(&mut stream).await?;
            return Ok((Some(header), Prefixed::new(Vec::new(), stream)));
        }
    }
    Ok((None, Prefixed::new(buf, stream)))
}

async fn read_v1<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Header> {
    let mut line = V1_SIGNATURE.to_vec();
    loop {
        let mut b = [0u8; 1];
        if stream.read(&mut b).await? == 0 {
            bail!("proxyproto: unexpected EOF in v1 header");
        }
        line.push(b[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
        if line.len() > V1_MAX_LEN {
            bail!("proxyproto: v1 header too long");
        }
    }
    let line = std::str::from_utf8(&line[..line.len() - 2])?;
    let parts: Vec<&str> = line.split(' ').collect();
    match parts.get(1) {
        Some(&"UNKNOWN") => return Ok(Header::default()),
        Some(&"TCP4") | Some(&"TCP6") if parts.len() == 6 => {}
        _ => bail!("proxyproto: invalid v1 header"),
    }
    let src_ip: IpAddr = parts[2].parse()?;
    let dst_ip: IpAddr = parts[3].parse()?;
    let src_port: u16 = parts[4].parse()?;
    let dst_port: u16 = parts[5].parse()?;
    Ok(Header {
        source: Some(SocketAddr::new(src_ip, src_port)),
        destination: Some(SocketAddr::new(dst_ip, dst_port)),
        client_cert: None,
    })
}

async fn read_v2<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Header> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    let ver_cmd = hdr[0];
    let fam = hdr[1];
    let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
    if ver_cmd >> 4 != 2 {
        bail!("proxyproto: unsupported version");
    }
    if len > V2_MAX_LEN {
        bail!("proxyproto: v2 header too long");
    }
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data).await?;

    let mut header = Header::default();
    let local = ver_cmd & 0x0f == 0;
    let (addr_len, addrs) = match fam >> 4 {
        1 if data.len() >= 12 => {
            let src = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
            let dst = Ipv4Addr::new(data[4], data[5], data[6], data[7]);
            let sp = u16::from_be_bytes([data[8], data[9]]);
            let dp = u16::from_be_bytes([data[10], data[11]]);
            (
                12,
                Some((
                    SocketAddr::new(src.into(), sp),
                    SocketAddr::new(dst.into(), dp),
                )),
            )
        }
        2 if data.len() >= 36 => {
            let src: [u8; 16] = data[0..16].try_into().unwrap();
            let dst: [u8; 16] = data[16..32].try_into().unwrap();
            let sp = u16::from_be_bytes([data[32], data[33]]);
            let dp = u16::from_be_bytes([data[34], data[35]]);
            (
                36,
                Some((
                    SocketAddr::new(Ipv6Addr::from(src).into(), sp),
                    SocketAddr::new(Ipv6Addr::from(dst).into(), dp),
                )),
            )
        }
        3 if data.len() >= 216 => (216, None),
        0 => (0, None),
        _ => bail!("proxyproto: invalid v2 address block"),
    };
    if !local && let Some((src, dst)) = addrs {
        header.source = Some(src);
        header.destination = Some(dst);
    }

    for (typ, value) in split_tlvs(&data[addr_len..])? {
        if typ == PP2_TYPE_SSL && value.len() >= 5 {
            for (sub, v) in split_tlvs(&value[5..])? {
                if sub == PP2_SUBTYPE_SSL_CLIENT_CERT {
                    header.client_cert = Some(v.to_vec());
                }
            }
        }
    }
    Ok(header)
}

fn split_tlvs(mut b: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    let mut tlvs = Vec::new();
    while !b.is_empty() {
        if b.len() < 3 {
            bail!("proxyproto: malformed TLV");
        }
        let typ = b[0];
        let len = u16::from_be_bytes([b[1], b[2]]) as usize;
        if b.len() < 3 + len {
            bail!("proxyproto: malformed TLV");
        }
        tlvs.push((typ, &b[3..3 + len]));
        b = &b[3 + len..];
    }
    Ok(tlvs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn parse(input: &[u8]) -> (Option<Header>, Vec<u8>) {
        let (h, mut rest) = read_header(input).await.unwrap();
        let mut out = Vec::new();
        rest.read_to_end(&mut out).await.unwrap();
        (h, out)
    }

    #[tokio::test]
    async fn no_header() {
        let (h, rest) = parse(b"PASS x\r\nNICK a\r\n").await;
        assert!(h.is_none());
        assert_eq!(rest, b"PASS x\r\nNICK a\r\n");
        let (h, rest) = parse(b"\r\nNICK a\r\n").await;
        assert!(h.is_none());
        assert_eq!(rest, b"\r\nNICK a\r\n");
    }

    #[tokio::test]
    async fn v1() {
        let (h, rest) = parse(b"PROXY TCP4 1.2.3.4 5.6.7.8 1111 6667\r\nNICK a\r\n").await;
        let h = h.unwrap();
        assert_eq!(h.source, Some("1.2.3.4:1111".parse().unwrap()));
        assert_eq!(h.destination, Some("5.6.7.8:6667".parse().unwrap()));
        assert_eq!(rest, b"NICK a\r\n");
        let (h, _) = parse(b"PROXY UNKNOWN\r\n").await;
        assert_eq!(h.unwrap().source, None);
    }

    #[tokio::test]
    async fn v2() {
        let cert = b"fake-der";
        let mut ssl = vec![0x01, 0, 0, 0, 0];
        ssl.push(PP2_SUBTYPE_SSL_CLIENT_CERT);
        ssl.extend_from_slice(&(cert.len() as u16).to_be_bytes());
        ssl.extend_from_slice(cert);
        let mut body = vec![10, 0, 0, 1, 10, 0, 0, 2, 0x04, 0xd2, 0x1a, 0x0b];
        body.push(PP2_TYPE_SSL);
        body.extend_from_slice(&(ssl.len() as u16).to_be_bytes());
        body.extend_from_slice(&ssl);
        let mut input = V2_SIGNATURE.to_vec();
        input.extend_from_slice(&[0x21, 0x11]);
        input.extend_from_slice(&(body.len() as u16).to_be_bytes());
        input.extend_from_slice(&body);
        input.extend_from_slice(b"NICK a\r\n");
        let (h, rest) = parse(&input).await;
        let h = h.unwrap();
        assert_eq!(h.source, Some("10.0.0.1:1234".parse().unwrap()));
        assert_eq!(h.destination, Some("10.0.0.2:6667".parse().unwrap()));
        assert_eq!(h.client_cert.as_deref(), Some(&cert[..]));
        assert_eq!(rest, b"NICK a\r\n");
    }
}
