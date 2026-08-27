//! 16-byte domain separation tag for signed post-handshake messages
//! per `docs/ARCHITECTURE.md` section 18.9.
//!
//! Layout:
//!
//! - bytes 0..8  : the ASCII bytes `b"locast/v1"` truncated to 8,
//!   i.e. `b"locast/v"`. The architecture says "the first 8 bytes
//!   are the ASCII string `locast/v1` (padded with NULs to 8)".
//!   Because `locast/v1` is 9 ASCII bytes, we take the first 8
//!   (`locast/v`) which fits the 8-byte prefix exactly. The
//!   post-handshake signing pipeline is not yet active in P2-T02
//!   so this choice is locked in here and must be matched by
//!   the corresponding encoder when the pipeline lands.
//! - bytes 8..16 : the UTF-8 message type name (e.g. `"CHAT_MSG"`),
//!   truncated or NUL-padded to fit.
//!
//! The handshake itself (§20.4.4) does not use this tag: the client
//! signs the raw 32-byte nonce and the server verifies with the
//! matching context. The tag is reserved for the post-handshake
//! envelope signing pipeline.

#![forbid(unsafe_code)]

const PREFIX: [u8; 8] = *b"locast/v";
const TAG_LEN: usize = 16;

/// Build the 16-byte domain tag for a post-handshake message type.
pub fn build(message_type: &str) -> [u8; 16] {
    let mut out = [0u8; TAG_LEN];
    out[..PREFIX.len()].copy_from_slice(&PREFIX);
    let type_bytes = message_type.as_bytes();
    let copy_len = type_bytes.len().min(TAG_LEN - PREFIX.len());
    out[PREFIX.len()..PREFIX.len() + copy_len].copy_from_slice(&type_bytes[..copy_len]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_length_for_short_type() {
        let tag = build("AUTH");
        assert_eq!(tag.len(), 16);
        assert_eq!(&tag[..8], b"locast/v");
        assert_eq!(&tag[8..12], b"AUTH");
        assert_eq!(&tag[12..16], &[0u8; 4]);
    }

    #[test]
    fn truncates_long_type_names() {
        // A type name longer than 8 bytes is truncated to 8.
        let tag = build("VERY_LONG_TYPE_NAME");
        assert_eq!(tag.len(), 16);
        assert_eq!(&tag[..8], b"locast/v");
        assert_eq!(&tag[8..16], b"VERY_LON");
    }

    #[test]
    fn empty_type_name_is_all_nul_suffix() {
        let tag = build("");
        assert_eq!(tag, {
            let mut t = [0u8; 16];
            t[..8].copy_from_slice(b"locast/v");
            t
        });
    }

    #[test]
    fn chat_msg_known_tag() {
        let tag = build("CHAT_MSG");
        assert_eq!(tag[..8], *b"locast/v");
        assert_eq!(&tag[8..16], b"CHAT_MSG");
    }
}
