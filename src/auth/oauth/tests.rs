use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

struct Mock {
    source: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl Mock {
    async fn new(handler: impl Fn(&str, &str) -> String + Send + 'static) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = format!("http://{address}");
        let source = format!("http+insecure://{address}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server_origin = origin.clone();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    accepted = listener.accept() => accepted?,
                    () = stopped.cancelled() => return Ok(()),
                };
                let mut stream = BufReader::new(stream);
                let request = read_request(&mut stream).await?;
                crate::state_lock::lock(&captured).push(request.clone());
                let response = handler(&server_origin, &request);
                if let Err(error) = stream.get_mut().write_all(response.as_bytes()).await {
                    if matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) { continue; }
                    return Err(error.into());
                }
            }
        });
        Ok(Self { source, requests, stop, task: Some(task) })
    }
    async fn shutdown(mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() { task.await??; }
        Ok(())
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() { task.abort(); }
    }
}
async fn read_request(stream: &mut BufReader<TcpStream>) -> Result<String> {
    let mut request = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 || line == "\r\n" { break; }
        if let Some((name, value)) = line.split_once(':') && name.eq_ignore_ascii_case("content-length") { length = value.trim().parse::<usize>()?; }
        request.push_str(&line);
        if request.len() > MAX_RESPONSE_BYTES { bail!("oversized fixture request headers"); }
    }
    if length > MAX_RESPONSE_BYTES { bail!("oversized fixture request body"); }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    request.push_str("\r\n");
    request.push_str(&String::from_utf8(body)?);
    Ok(request)
}
fn response(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
fn metadata(origin: &str, suffix: &str) -> String {
    serde_json::json!({"issuer":format!("{origin}{suffix}"), "introspection_endpoint":format!("{origin}/introspect"), "introspection_endpoint_auth_methods_supported":["none","client_secret_basic"]}).to_string()
}
async fn new_error(source: &str) -> Result<anyhow::Error> {
    match OAuth2::new(source).await { Err(error) => Ok(error), Ok(_) => bail!("invalid OAuth configuration was accepted") }
}
fn assert_redacted(error: &anyhow::Error) {
    let text = format!("{error:#?}");
    assert!(!text.contains("unit-secret"), "client secret escaped into auth diagnostics");
    assert!(!text.contains("unit-token"), "token escaped into auth diagnostics");
}

#[tokio::test]
async fn matching_issuer_credentials_forwarding_and_external_plain_auth() -> Result<()> {
    let mock = Mock::new(|origin, request| {
        if request.starts_with("GET ") { return response("200 OK", &metadata(origin, "/realm/")); }
        if request.contains("token=inactive") { response("200 OK", r#"{"active":false}"#) }
        else { response("200 OK", r#"{"active":true,"username":"alice"}"#) }
    }).await?;
    let source = mock.source.replacen("://", "://id:unit-secret@", 1) + "/realm/";
    let oauth = Arc::new(OAuth2::new(&source).await?);
    assert_eq!(oauth.auth_oauth_bearer("unit-token+value", "1.2.3.4:1234").await?, "alice");
    let error = oauth.auth_oauth_bearer("inactive", "").await.err().ok_or_else(|| anyhow!("inactive token accepted"))?;
    assert_eq!(super::super::auth_error_reason(&error), "Invalid credentials");
    assert_redacted(&error);
    let plain = super::super::PlainAuthenticator::OAuthPlain(oauth);
    let db = Arc::new(crate::database::Database::open_in_memory()?);
    plain.auth_plain(&db, "alice", "unit-token", "").await?;
    let error = plain.auth_plain(&db, "other", "unit-token", "").await.err().ok_or_else(|| anyhow!("mismatched username accepted"))?;
    assert_redacted(&error);
    {
        let requests = crate::state_lock::lock(&mock.requests);
        let first = requests.first().ok_or_else(|| anyhow!("no discovery request"))?;
        assert!(first.starts_with("GET /.well-known/oauth-authorization-server/realm HTTP/1.1"));
        assert!(!first.to_ascii_lowercase().contains("authorization:"));
        let post = requests.get(1).ok_or_else(|| anyhow!("no introspection request"))?;
        assert!(post.contains("token=unit-token%2Bvalue"));
        assert!(post.to_ascii_lowercase().contains("x-forwarded-for: 1.2.3.4"));
        assert!(post.contains(&BASE64.encode("id:unit-secret")));
    }
    mock.shutdown().await
}

#[tokio::test]
async fn issuer_mismatch_and_missing_issuer_never_use_metadata() -> Result<()> {
    for suffix in ["/other", "/realm/", "", "/unit-secret"] {
        let mock = Mock::new(move |origin, _| response("200 OK", &metadata(origin, suffix))).await?;
        let error = new_error(&(mock.source.clone() + "/realm")).await?;
        assert_redacted(&error);
        assert_eq!(crate::state_lock::lock(&mock.requests).len(), 1);
        mock.shutdown().await?;
    }
    let mock = Mock::new(|origin, _| response("200 OK", &serde_json::json!({"introspection_endpoint":format!("{origin}/introspect")}).to_string())).await?;
    assert!(new_error(&mock.source).await?.to_string().contains("issuer"));
    mock.shutdown().await
}

#[test]
fn issuer_identity_and_endpoint_transport_policy() -> Result<()> {
    let issuer = Issuer::parse("https://id:unit-secret@EXAMPLE.org:443/%72ealm/")?;
    assert_eq!(issuer.expected, "https://EXAMPLE.org:443/%72ealm/");
    assert!(issuer.endpoint("https://different.example/introspect?provider=1").is_ok());
    for endpoint in ["http://example.org/introspect", "/relative", "https://id:unit-secret@example.org/introspect", "https://example.org/introspect#fragment"] { assert!(issuer.endpoint(endpoint).is_err()); }
    for source in ["http://127.0.0.1/realm", "http+insecure://localhost/realm", "http+insecure://192.0.2.1/realm", "https://example.org?query", "https://example.org#fragment", "https://example.org\\path", "https://:unit-secret@example.org"] {
        let error = Issuer::parse(source).err().ok_or_else(|| anyhow!("invalid issuer accepted"))?;
        assert_redacted(&error);
    }
    let issuer = Issuer::parse("http+insecure://127.0.0.1:1234/realm")?;
    assert!(issuer.endpoint("http://127.0.0.1:1234/introspect").is_ok());
    assert!(issuer.endpoint("http://127.0.0.1:1235/introspect").is_err());
    assert!(issuer.endpoint("https://other.example/introspect").is_err());
    assert!(Issuer::parse("http+insecure://[::1]:1234/realm").is_ok());
    Ok(())
}

#[tokio::test]
async fn redirects_are_rejected_for_metadata_and_token_posts() -> Result<()> {
    for discovery in [true, false] {
        let mock = Mock::new(move |origin, request| {
            if discovery || request.starts_with("POST ") {
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/redirect?unit-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else { response("200 OK", &metadata(origin, "")) }
        }).await?;
        let error = if discovery { new_error(&mock.source).await? }
            else { OAuth2::new(&mock.source).await?.auth_oauth_bearer("unit-token", "").await.err().ok_or_else(|| anyhow!("redirect accepted"))? };
        assert!(error.to_string().contains("307"));
        assert_redacted(&error);
        assert_eq!(crate::state_lock::lock(&mock.requests).len(), if discovery {1} else {2});
        mock.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn oversized_known_and_chunked_metadata_are_rejected() -> Result<()> {
    for chunked in [false, true] {
        let mock = Mock::new(move |_, _| {
            if chunked {
                let body = " ".repeat(MAX_RESPONSE_BYTES + 1);
                format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len())
            } else { format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_RESPONSE_BYTES + 1) }
        }).await?;
        let error = new_error(&mock.source).await?;
        assert!(error.to_string().contains("limit"));
        mock.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn introspection_errors_are_bounded_and_never_quote_provider_values() -> Result<()> {
    for mode in 0..5 {
        let mock = Mock::new(move |origin, request| {
            if request.starts_with("GET ") { return response("200 OK", &metadata(origin, "")); }
            match mode {
                0 => response("200 OK", r#"{"active":"unit-token unit-secret","username":"alice"}"#),
                1 => response("200 OK", &format!(r#"{{"active":true,"username":"{}"}}"#, "x".repeat(MAX_RESPONSE_BYTES))),
                2 => "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10001\r\n".to_owned() + &" ".repeat(MAX_RESPONSE_BYTES + 1) + "\r\n0\r\n\r\n",
                3 => response("401 Unauthorized", "unit-token unit-secret"),
                _ => response("200 OK", r#"{"active":true}"#),
            }
        }).await?;
        let oauth = OAuth2::new(&mock.source).await?;
        let error = oauth.auth_oauth_bearer("unit-token", "").await.err().ok_or_else(|| anyhow!("invalid introspection response accepted"))?;
        assert_redacted(&error);
        if mode == 1 || mode == 2 { assert!(error.to_string().contains("limit")); }
        mock.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn https_discovery_keeps_normal_certificate_verification() -> Result<()> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])?;
    let certificate = params.self_signed(&key)?;
    let config = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
        .with_safe_default_protocol_versions()?.with_no_client_auth()
        .with_single_cert(vec![certificate.der().clone()], rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        assert!(tokio_rustls::TlsAcceptor::from(Arc::new(config)).accept(stream).await.is_err());
        Ok::<(), anyhow::Error>(())
    });
    let error = new_error(&format!("https://id:unit-secret@{address}")).await?;
    assert_redacted(&error);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn local_metadata_cannot_delegate_tokens_to_another_origin() -> Result<()> {
    for endpoint in ["http://127.0.0.1:1/token", "https://other.example/token", "http://id:unit-secret@127.0.0.1/token"] {
        let mock = Mock::new(move |origin, _| response("200 OK", &serde_json::json!({"issuer":origin,"introspection_endpoint":endpoint}).to_string())).await?;
        let error = new_error(&mock.source).await?;
        assert_redacted(&error);
        assert_eq!(crate::state_lock::lock(&mock.requests).len(), 1);
        mock.shutdown().await?;
    }
    Ok(())
}
