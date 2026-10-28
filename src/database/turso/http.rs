//! A dedicated HTTP worker keeps the synchronous storage interface usable
//! from both Tokio runtime flavors, including the single-threaded server.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde_json::Value;

struct Request {
    method: Method,
    url: String,
    token: String,
    body: Option<Value>,
    reply: mpsc::SyncSender<Result<(StatusCode, Value)>>,
}

#[derive(Clone)]
pub(in crate::database) struct Http(tokio::sync::mpsc::UnboundedSender<Request>);

impl Http {
    pub fn new() -> Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("turso-http".into())
            .spawn(move || {
                let setup = (|| -> Result<_> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let client = rt.block_on(async {
                        reqwest::Client::builder()
                            .timeout(Duration::from_secs(30))
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                    })?;
                    Ok((rt, client))
                })();
                let (rt, client) = match setup {
                    Ok(pair) => {
                        let _ = ready_tx.send(Ok(()));
                        pair
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                rt.block_on(async {
                    let mut tasks = tokio::task::JoinSet::new();
                    while let Some(request) = rx.recv().await {
                        let client = client.clone();
                        tasks.spawn(async move {
                            let result = async {
                                let mut req = client
                                    .request(request.method, &request.url)
                                    .bearer_auth(&request.token);
                                if let Some(body) = request.body {
                                    req = req.json(&body);
                                }
                                let response = req.send().await.map_err(|e| {
                                    if e.is_timeout() {
                                        anyhow!("Turso HTTP request timed out")
                                    } else {
                                        e.into()
                                    }
                                })?;
                                let status = response.status();
                                // Do not include response bodies in HTTP errors: these may
                                // contain SQL arguments or credentials returned by a proxy.
                                if !status.is_success() {
                                    return Ok((status, Value::Null));
                                }
                                let bytes = response.bytes().await?;
                                Ok((
                                    status,
                                    if bytes.is_empty() {
                                        Value::Null
                                    } else {
                                        serde_json::from_slice(&bytes)?
                                    },
                                ))
                            }
                            .await;
                            let _ = request.reply.send(result);
                        });
                        while tasks.try_join_next().is_some() {}
                    }
                    while tasks.join_next().await.is_some() {}
                });
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("Turso HTTP worker failed to start"))??;
        Ok(Self(tx))
    }

    pub fn request(
        &self,
        method: Method,
        url: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0
            .send(Request {
                method,
                url: url.into(),
                token: token.into(),
                body,
                reply: tx,
            })
            .map_err(|_| anyhow!("Turso HTTP worker stopped"))?;
        rx.recv()
            .map_err(|_| anyhow!("Turso HTTP worker stopped"))?
    }

    pub fn json(
        &self,
        method: Method,
        url: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let (status, value) = self.request(method, url, token, body)?;
        if !status.is_success() {
            bail!("Turso request failed (HTTP {status})");
        }
        Ok(value)
    }
}
