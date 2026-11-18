//! Admission is attached to the physical stream, including after HTTP upgrades.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::conn::BoxIo;

#[derive(Default)]
pub(crate) struct SessionBudget(AtomicUsize);

impl SessionBudget {
    pub(crate) fn active(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn admit(self: &Arc<Self>, limit: usize, io: BoxIo) -> Option<BoxIo> {
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                (active < limit).then(|| active + 1)
            })
            .ok()?;
        Some(Box::new(AdmittedIo {
            inner: io,
            _permit: SessionPermit(self.clone()),
        }))
    }
}

struct SessionPermit(Arc<SessionBudget>);

impl Drop for SessionPermit {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct AdmittedIo {
    inner: BoxIo,
    _permit: SessionPermit,
}

impl AsyncRead for AdmittedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for AdmittedIo {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_admission_never_exceeds_limit() -> anyhow::Result<()> {
        let budget = Arc::new(SessionBudget::default());
        let arrived = Arc::new(tokio::sync::Barrier::new(33));
        let release = Arc::new(tokio::sync::Barrier::new(33));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let (budget, arrived, release) = (budget.clone(), arrived.clone(), release.clone());
            tasks.push(tokio::spawn(async move {
                let (io, _peer) = tokio::io::duplex(64);
                let admitted = budget.admit(3, Box::new(io));
                arrived.wait().await;
                release.wait().await;
                drop(admitted);
            }));
        }
        arrived.wait().await;
        assert_eq!(budget.active(), 3);
        release.wait().await;
        for task in tasks {
            task.await?;
        }
        assert_eq!(budget.active(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn aborted_downstream_releases_transport_admission() -> anyhow::Result<()> {
        use crate::conn::{Conn, ConnInfo};
        use crate::database::Database;
        use crate::logger::Logger;
        use crate::server::Server;
        use std::time::Duration;

        let budget = Arc::new(SessionBudget::default());
        let (io, _peer) = tokio::io::duplex(64);
        let io = budget
            .admit(1, Box::new(io))
            .ok_or_else(|| anyhow::anyhow!("no admission"))?;
        let logger = Logger::new(false);
        let server = Server::new(Database::open_in_memory()?, logger.clone());
        let (conn, reader) = Conn::new_stream(io, logger.clone(), None);
        let task = tokio::spawn(crate::downstream::serve(
            server,
            conn,
            reader,
            ConnInfo::default(),
            logger,
        ));
        tokio::task::yield_now().await;
        assert_eq!(budget.active(), 1);
        task.abort();
        assert!(task.await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while budget.active() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }
}
