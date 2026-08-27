//! Prometheus metrics for the Locast signaling server.
//!
//! P0-T03 ships the exporter skeleton: the default registry is exposed at
//! `GET /metrics` and returns an empty body (200 OK) until P2+ adds the
//! connection, room, and per-command counters described in
//! `docs/ARCHITECTURE.md` section 20.

use std::sync::Arc;

use prometheus::Encoder;

/// Shared metrics handle. The handle owns the default Prometheus registry
/// and a text encoder used by the `/metrics` HTTP handler.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    registry: prometheus::Registry,
    encoder: prometheus::TextEncoder,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Create a new `Metrics` backed by a fresh Prometheus registry. The
    /// registry is intentionally empty in P0-T03; later phases register
    /// counters, gauges, and histograms via `prometheus::Counter::with_opts!`
    /// on the same registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                registry: prometheus::Registry::new(),
                encoder: prometheus::TextEncoder::new(),
            }),
        }
    }

    /// Render the current registry to a text-format payload suitable for
    /// `GET /metrics`. The body is empty (200 OK) until counters are
    /// registered in a later phase.
    pub fn render(&self) -> Result<(String, &'static str), MetricsError> {
        let mut buf = Vec::new();
        self.inner
            .encoder
            .encode(&self.inner.registry.gather(), &mut buf)
            .map_err(MetricsError::Encode)?;
        let body = String::from_utf8(buf).map_err(MetricsError::Utf8)?;
        Ok((body, "text/plain; version=0.0.4"))
    }
}

/// Errors raised while rendering metrics.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("failed to encode Prometheus metrics: {0}")]
    Encode(prometheus::Error),

    #[error("Prometheus payload was not valid UTF-8: {0}")]
    Utf8(std::string::FromUtf8Error),
}
