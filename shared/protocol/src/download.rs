//! Download resume wire protocol (P7-T04).
//!
//! Defines the envelope types for download pause/resume coordination.
//! The server is a pure relay for download coordination; the client
//! drives the resume logic locally from the persisted `download_chunks`
//! bitmap. These types enable future server-assisted resume if needed.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Client → Server: request to pause an in-flight download.
/// The server ACKs and stops forwarding chunks for this download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadPauseRequestPayload {
    pub download_id: String,
    /// The chunk index the viewer was on when pausing (for progress display)
    pub paused_at_chunk: u32,
}

/// Server → Client: pause acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadPauseResponsePayload {
    pub download_id: String,
    pub accepted: bool,
}

/// Client → Server: request to resume a paused download.
/// Carries the completed chunk bitmap so the source can skip verified chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadResumeRequestPayload {
    pub download_id: String,
    /// LSB-first bitmap of verified + received chunks.
    /// Chunk `i` is at `bitmap[i / 8] & (1 << (i % 8))`.
    pub have_chunks: Vec<u8>,
    /// Optional resume token for multi-source coordination (future).
    pub resume_token: Option<String>,
}

/// Server → Client: resume response with the next chunk to send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadResumeResponsePayload {
    pub download_id: String,
    pub accepted: bool,
    /// Next chunk index to send (0 if all chunks done)
    pub next_chunk_index: u32,
    /// Total bytes remaining
    pub remaining_bytes: u64,
    /// ETag of the source file for range request validation
    pub etag: Option<String>,
}

/// Client → Server: cancel a download entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadCancelRequestPayload {
    pub download_id: String,
}

/// Server → Client: cancel acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadCancelResponsePayload {
    pub download_id: String,
    pub accepted: bool,
}

/// Client → Server: list active downloads for the current room/user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadListRequestPayload {
    pub room_id: Option<String>,
}

/// Server → Client: list response with download summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadListResponsePayload {
    pub downloads: Vec<DownloadSummaryIpc>,
}

/// Summary of a download for list responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct DownloadSummaryIpc {
    pub id: String,
    pub media_id: String,
    pub room_id: Option<String>,
    pub state: DownloadStateIpc,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub source_peer_id: Option<String>,
    pub chunk_size_bytes: u32,
    pub manifest_version: i64,
}

/// Download state as sent over the wire (matches DownloadState in state.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "ts/index.ts")]
pub enum DownloadStateIpc {
    Pending,
    Connecting,
    Transferring,
    Verifying,
    Complete,
    Failed,
    Paused,
    Cancelled,
}

impl DownloadStateIpc {
    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadStateIpc::Pending => "pending",
            DownloadStateIpc::Connecting => "connecting",
            DownloadStateIpc::Transferring => "transferring",
            DownloadStateIpc::Verifying => "verifying",
            DownloadStateIpc::Complete => "complete",
            DownloadStateIpc::Failed => "failed",
            DownloadStateIpc::Paused => "paused",
            DownloadStateIpc::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn download_state_ipc_roundtrip() {
        let state = DownloadStateIpc::Paused;
        let json = serde_json::to_string(&state).expect("serialize");
        let decoded: DownloadStateIpc = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, state);
    }

    #[test]
    fn download_pause_request_roundtrip() {
        let req = DownloadPauseRequestPayload {
            download_id: Uuid::new_v4().to_string(),
            paused_at_chunk: 42,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: DownloadPauseRequestPayload =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn download_resume_request_bitmap() {
        // 10 chunks: indices 0, 2, 5 verified
        // bitmap: byte 0 = 0b00100101 = 0x25, byte 1 = 0
        let mut bitmap = [0u8; 2];
        bitmap[0] |= 1 << 0; // chunk 0
        bitmap[0] |= 1 << 2; // chunk 2
        bitmap[1] |= 1 << 5; // chunk 5 (bit 5 of byte 0? wait, chunk 5 = byte 0, bit 5)
                             // Actually: chunk i is at bitmap[i/8] & (1 << (i%8))
                             // chunk 0: byte 0, bit 0
                             // chunk 2: byte 0, bit 2
                             // chunk 5: byte 0, bit 5
                             // chunk 8: byte 1, bit 0
                             // So byte 0 = 0b00100101 = 0x25
        let req = DownloadResumeRequestPayload {
            download_id: Uuid::new_v4().to_string(),
            have_chunks: bitmap.to_vec(),
            resume_token: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: DownloadResumeRequestPayload =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.have_chunks, vec![0x25, 0]);
    }
}
