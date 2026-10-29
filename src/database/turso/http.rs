//! A dedicated HTTP worker keeps the synchronous storage interface usable
//! from both Tokio runtime flavors, including the single-threaded server.

use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
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
pub(in crate::database) struct Http(tokio::sync::mpsc::UnboundedSender<Request>, Arc<Usage>);

#[derive(Default)]
struct Usage {
    read: AtomicU64,
    written: AtomicU64,
}
impl Usage {
    fn observe(&self, value: &Value) {
        let mut read = 0;
        let mut written = 0;
        let mut add = |result: &Value| {
            read += result["rows_read"].as_u64().unwrap_or(0);
            written += result["rows_written"].as_u64().unwrap_or(0);
        };
        for result in value["results"].as_array().into_iter().flatten() {
            let result = &result["response"]["result"];
            if let Some(steps) = result["step_results"].as_array() {
                for step in steps {
                    add(step);
                }
            } else {
                add(result);
            }
        }
        self.read.fetch_add(read, Ordering::Relaxed);
        self.written.fetch_add(written, Ordering::Relaxed);
    }
}

impl Http {
    pub fn new() -> Result<Self> {
        let usage = Arc::new(Usage::default());
        let worker_usage = usage.clone();
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
                        let usage = worker_usage.clone();
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
                                let value = if bytes.is_empty() {
                                    Value::Null
                                } else {
                                    serde_json::from_slice(&bytes)?
                                };
                                usage.observe(&value);
                                Ok((status, value))
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
        Ok(Self(tx, usage))
    }

    pub fn observed_usage(&self) -> (u64, u64) {
        (
            self.1.read.load(Ordering::Relaxed),
            self.1.written.load(Ordering::Relaxed),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn usage_counts_execute_and_batch_results_once() {
        let usage = Usage::default();
        usage.observe(&json!({"results":[
            {"type":"ok","response":{"type":"execute","result":{"rows_read":2,"rows_written":3}}},
            {"type":"ok","response":{"type":"batch","result":{"rows_read":999,"step_results":[{"rows_read":5,"rows_written":7},null,{"rows_read":11,"rows_written":13}]}}},
            {"type":"error","error":{"code":"SQLITE_ERROR"}},
            {"type":"ok","response":{"type":"close"}}
        ]}));
        assert_eq!(usage.read.load(Ordering::Relaxed), 18);
        assert_eq!(usage.written.load(Ordering::Relaxed), 23);
        usage.observe(&Value::Null);
        assert_eq!(usage.read.load(Ordering::Relaxed), 18);
    }
}
