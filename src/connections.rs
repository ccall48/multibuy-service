//! Track gRPC client connections: when each opened, when and why it closed.
//!
//! HPR sends every multibuy request for a route over one long-lived HTTP/2
//! connection. When that connection drops, HPR's next request fails, and with
//! `fail_on_unavailable` set it backs off and drops packets (see
//! [`crate::traffic`]). Reconnects are therefore the thing to line up against
//! silences: a new connection just after each gap points at the link, while
//! gaps on one steady connection point elsewhere.

use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::{Stream, StreamExt};
use tonic::transport::server::{Connected, TcpConnectInfo, TcpIncoming};

use crate::tasks::grpc_server::HTTP2_KEEPALIVE_TIMEOUT;

/// How many open/close events are kept for the API.
const MAX_EVENTS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Opened,
    Closed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionEvent {
    /// Unix seconds.
    pub at: u64,
    pub id: u64,
    pub peer: String,
    pub kind: EventKind,
    /// How long the connection was open (closes only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_seconds: Option<u64>,
    /// Why it closed (closes only). One of:
    /// - "peer closed": we read EOF from the client first.
    /// - "closed cleanly": an HTTP/2-level close (a client GOAWAY, or our
    ///   shutdown) with the client still talking to us.
    /// - "peer unresponsive": nothing heard from the client for at least the
    ///   keepalive timeout before the close — the link or client went dead.
    /// - an I/O error kind, e.g. "ConnectionReset" or "TimedOut".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Seconds since we last received bytes from the client (closes only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_seconds: Option<u64>,
}

/// Counters and a bounded log of recent connection events.
#[derive(Default)]
pub struct Connections {
    next_id: AtomicU64,
    active: AtomicU64,
    opened: AtomicU64,
    /// Touched only when a connection opens or closes, never per request.
    events: Mutex<VecDeque<ConnectionEvent>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Connections {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    pub fn opened_total(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }

    /// Recent events, oldest first.
    pub fn events(&self) -> Vec<ConnectionEvent> {
        self.lock_events().iter().cloned().collect()
    }

    fn lock_events(&self) -> std::sync::MutexGuard<'_, VecDeque<ConnectionEvent>> {
        // The log is diagnostic; a panic elsewhere while holding it shouldn't
        // take connection tracking down with it.
        self.events.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, event: ConnectionEvent) {
        let mut events = self.lock_events();
        if events.len() == MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    fn opened(&self, peer: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.active.fetch_add(1, Ordering::Relaxed);
        self.opened.fetch_add(1, Ordering::Relaxed);
        tracing::info!(id, peer, "gRPC connection opened");
        self.push(ConnectionEvent {
            at: now_unix(),
            id,
            peer: peer.to_string(),
            kind: EventKind::Opened,
            open_seconds: None,
            reason: None,
            idle_seconds: None,
        });
        id
    }

    fn closed(&self, id: u64, peer: &str, open_seconds: u64, idle_seconds: u64, reason: String) {
        self.active.fetch_sub(1, Ordering::Relaxed);
        tracing::info!(
            id,
            peer,
            open_seconds,
            idle_seconds,
            reason,
            "gRPC connection closed"
        );
        self.push(ConnectionEvent {
            at: now_unix(),
            id,
            peer: peer.to_string(),
            kind: EventKind::Closed,
            open_seconds: Some(open_seconds),
            reason: Some(reason),
            idle_seconds: Some(idle_seconds),
        });
    }
}

/// Accept connections from `listener` with TCP keepalive set, recording each
/// one's lifetime in `connections`.
pub fn tracked_incoming(
    listener: TcpListener,
    connections: Arc<Connections>,
    tcp_keepalive: Option<std::time::Duration>,
) -> impl Stream<Item = io::Result<TrackedStream>> {
    TcpIncoming::from(listener)
        .with_nodelay(Some(true))
        .with_keepalive(tcp_keepalive)
        .map(move |accepted| accepted.map(|stream| TrackedStream::new(stream, connections.clone())))
}

/// A `TcpStream` that reports its open and close to [`Connections`].
///
/// The close reason is the first thing that ended it: the peer's EOF or an I/O
/// error. Otherwise hyper closed it, which at this layer looks the same whether
/// the client sent GOAWAY or a keepalive ping went unanswered — so the two are
/// told apart by how long the client had been silent.
pub struct TrackedStream {
    inner: TcpStream,
    connections: Arc<Connections>,
    id: u64,
    peer: String,
    opened_at: Instant,
    last_read: Instant,
    reason: Option<String>,
}

impl TrackedStream {
    fn new(inner: TcpStream, connections: Arc<Connections>) -> Self {
        let peer = inner
            .peer_addr()
            .map_or_else(|_| "unknown".to_string(), |a: SocketAddr| a.to_string());
        let id = connections.opened(&peer);
        Self {
            inner,
            connections,
            id,
            peer,
            opened_at: Instant::now(),
            last_read: Instant::now(),
            reason: None,
        }
    }

    fn note<T>(&mut self, result: &Poll<io::Result<T>>) {
        if let Poll::Ready(Err(e)) = result {
            self.reason.get_or_insert_with(|| format!("{:?}", e.kind()));
        }
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        let idle = self.last_read.elapsed();
        let reason = self.reason.take().unwrap_or_else(|| {
            if idle >= HTTP2_KEEPALIVE_TIMEOUT {
                "peer unresponsive".to_string()
            } else {
                "closed cleanly".to_string()
            }
        });
        self.connections.closed(
            self.id,
            &self.peer,
            self.opened_at.elapsed().as_secs(),
            idle.as_secs(),
            reason,
        );
    }
}

impl Connected for TrackedStream {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

impl AsyncRead for TrackedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            if buf.filled().len() == before {
                self.reason.get_or_insert_with(|| "peer closed".to_string());
            } else {
                self.last_read = Instant::now();
            }
        }
        self.note(&result);
        result
    }
}

impl AsyncWrite for TrackedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.note(&result);
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.note(&result);
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.note(&result);
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.note(&result);
        result
    }
}
