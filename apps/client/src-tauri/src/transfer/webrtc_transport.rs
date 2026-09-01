//! P3-T13: WebRtcTransport -- bridges a webrtc 0.20 DataChannel to the
//! `Transport` trait used by MultiSourceReceiver.
//!
//! Spawns a per-channel receive pump that pushes incoming bytes into
//! an mpsc channel; `recv()` awaits the next message. `send()` writes
//! bytes via the DataChannel's send API (which takes `BytesMut`).
//!
//! The transport is a thin adapter. All framing (length-prefix + JSON)
//! is done by `transfer::wire::codec`. The DataChannel itself is
//! already a binary-clean stream (no line discipline), so the adapter
//! shuttles raw bytes end-to-end.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use bytes::BytesMut;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{DataChannel, DataChannelEvent};

use crate::transfer::transport::{Transport, TransportError};

/// Adapter that exposes a webrtc 0.20 `Arc<dyn DataChannel>` as the
/// transfer layer's [`Transport`] trait. Bytes-in, bytes-out; no
/// framing layer lives here (the wire codec lives in
/// `transfer::wire::codec`).
pub struct WebRtcTransport {
    dc: Arc<dyn DataChannel>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    cancel: CancellationToken,
}

impl WebRtcTransport {
    /// Wrap `dc` in a Transport. Spawns a poll task that drains
    /// `DataChannelEvent::OnMessage` bytes into the internal mpsc.
    /// On `OnClose` or cancel, the task exits and `recv()` returns
    /// `Ok(None)`.
    pub fn new(dc: Arc<dyn DataChannel>, cancel: CancellationToken) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let dc2 = dc.clone();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel2.cancelled() => break,
                    ev = dc2.poll() => {
                        match ev {
                            Some(DataChannelEvent::OnMessage(msg)) => {
                                let bytes = msg.data.to_vec();
                                if tx.send(bytes).is_err() {
                                    // The receiver was dropped
                                    // (transport closed); exit.
                                    break;
                                }
                            }
                            Some(DataChannelEvent::OnClose) => break,
                            _ => {
                                // OnOpen / OnError / buffered-amount
                                // events: ignore. Yield to the
                                // scheduler so we don't busy-spin on
                                // a quiet channel.
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
            }
        });
        Self {
            dc,
            rx: Arc::new(Mutex::new(rx)),
            cancel,
        }
    }
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        let mut buf = BytesMut::with_capacity(frame_bytes.len());
        buf.extend_from_slice(&frame_bytes);
        self.dc
            .send(buf)
            .await
            .map_err(|e| TransportError::Io(format!("webrtc dc send: {e}")))?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut g = self.rx.lock().await;
        match g.recv().await {
            Some(b) => Ok(Some(b)),
            None => Ok(None),
        }
    }

    async fn close(&self) {
        let _ = self.dc.close().await;
        self.cancel.cancel();
    }
}
