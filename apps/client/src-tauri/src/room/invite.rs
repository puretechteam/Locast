//! `room::invite` - the room-invite URL parser (P3-T04
//! prerequisite 2).
//!
//! The host's `room_create` produces a URL of the form:
//!
//! ```text
//! locast://join/<room_code>?h=<base64url-no-pad-of-host-pubkey>&v=1
//! ```
//!
//! (Architecture §8 line 795.) The viewer's job is to extract
//! `<room_code>` and the raw 32-byte host pubkey from `h=`
//! and thread the pubkey into the [`crate::net::room::RoomClient`]
//! as the trust anchor for manifest verification.
//!
//! The parser is **strict**: it rejects any URL that does not
//! match the canonical shape. This is the trust boundary
//! between the host and the viewer; permissive parsing here
//! is a real risk.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use thiserror::Error;
use url::Url;

use locast_crypto::ed25519;

/// What the viewer extracted from a successful invite parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInvite {
    /// The 6-character room code.
    pub room_code: String,
    /// The host's raw 32-byte Ed25519 public key, decoded
    /// from the `h=` parameter.
    pub host_pubkey: [u8; 32],
    /// The invite `v=` parameter, if present. v1 is
    /// currently the only supported version.
    pub version: Option<u32>,
}

/// Errors raised by [`parse_invite`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InviteError {
    /// The URL is empty or has no host / scheme.
    #[error("not a locast invite url (missing scheme/host)")]
    NotLocast,
    /// The URL host is not the expected `join` literal.
    #[error("not a locast join URL (host={0:?})")]
    NotJoin(String),
    /// The first path segment is missing or empty.
    #[error("missing room code")]
    MissingRoomCode,
    /// The room code is not exactly 6 characters.
    #[error("room code must be 6 chars, got {0}")]
    BadRoomCodeLength(usize),
    /// The `h=` parameter is missing.
    #[error("missing h= parameter")]
    MissingH,
    /// The `h=` parameter did not decode as base64url no-pad.
    #[error("h= is not valid base64url-no-pad")]
    HDecode,
    /// The decoded `h=` is not exactly 32 bytes.
    #[error("h= must be 32 bytes, got {0}")]
    HWrongLength(usize),
    /// The URL did not parse at all.
    #[error("url parse error: {0}")]
    UrlParse(String),
}

/// Parse a `locast://join/<code>?h=<...>&v=1` invite URL.
/// Strict: any deviation is an error. The `scheme` argument
/// is the expected scheme (typically `locast`); both
/// `locast://` and `<scheme>://` are accepted so a
/// developer-mode `devlocast://` URL is also handled.
///
/// # Examples
///
/// ```text
/// parse_invite("locast", "locast://join/AAAAAA?h=d75a98...&v=1")
///   -> Ok(ParsedInvite { room_code: "AAAAAA", host_pubkey: <32 bytes>, version: Some(1) })
/// ```
pub fn parse_invite(scheme: &str, url: &str) -> Result<ParsedInvite, InviteError> {
    if url.is_empty() {
        return Err(InviteError::UrlParse("empty".into()));
    }
    let parsed = Url::parse(url).map_err(|e| InviteError::UrlParse(e.to_string()))?;
    if parsed.scheme() != scheme && parsed.scheme() != "locast" {
        return Err(InviteError::NotLocast);
    }
    // The path is `/<room_code>`.
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(InviteError::MissingRoomCode);
    }
    if segments[0].len() != 6 {
        return Err(InviteError::BadRoomCodeLength(segments[0].len()));
    }
    let room_code = segments[0].to_string();

    // h= and v= query params.
    let mut host_pubkey: Option<[u8; 32]> = None;
    let mut version: Option<u32> = None;
    for (k, v) in parsed.query_pairs() {
        if k == "h" {
            let bytes = ed25519::from_base64url_no_pad(&v).map_err(|_| InviteError::HDecode)?;
            if bytes.len() != 32 {
                return Err(InviteError::HWrongLength(bytes.len()));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            host_pubkey = Some(out);
        } else if k == "v" {
            version = v.parse::<u32>().ok();
        }
    }
    let host_pubkey = host_pubkey.ok_or(InviteError::MissingH)?;
    Ok(ParsedInvite {
        room_code,
        host_pubkey,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    /// RFC 8032 test 1 pubkey (32 bytes).
    const RFC8032_TEST1_PUBKEY: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];

    /// The exact base64url no-pad encoding of the above
    /// pubkey (43 chars, no padding).
    fn h_for_test1() -> String {
        URL_SAFE_NO_PAD.encode(RFC8032_TEST1_PUBKEY)
    }

    fn full_url(code: &str, h: &str, v: Option<&str>) -> String {
        match v {
            Some(v) => format!("locast://join/{code}?h={h}&v={v}"),
            None => format!("locast://join/{code}?h={h}"),
        }
    }

    #[test]
    fn parses_a_well_formed_invite() {
        let h = h_for_test1();
        let url = full_url("AAAAAA", &h, Some("1"));
        let inv = parse_invite("locast", &url).expect("parse");
        assert_eq!(inv.room_code, "AAAAAA");
        assert_eq!(inv.host_pubkey, RFC8032_TEST1_PUBKEY);
        assert_eq!(inv.version, Some(1));
    }

    #[test]
    fn accepts_invite_without_v() {
        let h = h_for_test1();
        let url = full_url("AAAAAA", &h, None);
        let inv = parse_invite("locast", &url).expect("parse");
        assert_eq!(inv.version, None);
        assert_eq!(inv.host_pubkey, RFC8032_TEST1_PUBKEY);
    }

    #[test]
    fn rejects_wrong_scheme() {
        let url = "https://join/AAAAAA?h=00&v=1";
        let err = parse_invite("locast", url).expect_err("must reject https");
        assert!(matches!(err, InviteError::NotLocast));
    }

    #[test]
    fn rejects_missing_room_code() {
        let url = "locast://join/?h=00";
        let err = parse_invite("locast", url).expect_err("missing code");
        assert!(matches!(err, InviteError::MissingRoomCode));
    }

    #[test]
    fn rejects_wrong_length_room_code() {
        let h = h_for_test1();
        let url = full_url("AAAAA", &h, Some("1")); // 5 chars
        let err = parse_invite("locast", &url).expect_err("5 chars must reject");
        assert!(matches!(err, InviteError::BadRoomCodeLength(5)));
    }

    #[test]
    fn rejects_missing_h() {
        let url = "locast://join/AAAAAA?v=1";
        let err = parse_invite("locast", url).expect_err("missing h");
        assert!(matches!(err, InviteError::MissingH));
    }

    #[test]
    fn rejects_h_with_standard_base64_padding() {
        // The 32-byte pubkey in standard padded base64 has a
        // `=` at the end. The strict parser must reject.
        let h = h_for_test1();
        let url = format!("locast://join/AAAAAA?h={h}=&v=1");
        let err = parse_invite("locast", &url).expect_err("padded must reject");
        assert!(matches!(err, InviteError::HDecode));
    }

    #[test]
    fn rejects_h_wrong_decoded_length() {
        // A short base64url blob that decodes to fewer than
        // 32 bytes. We don't need to know the exact byte
        // length — just that it is NOT 32.
        let url = "locast://join/AAAAAA?h=AAAA&v=1";
        let err = parse_invite("locast", url).expect_err("short h must reject");
        match err {
            InviteError::HWrongLength(n) => {
                assert!(n != 32, "decoded length must not be 32, got {n}");
            }
            InviteError::HDecode => { /* also acceptable */ }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn accepts_devlocast_scheme() {
        // The parser is also tolerant of the canonical
        // "locast" scheme in addition to whatever
        // `scheme` was passed.
        let h = h_for_test1();
        let url = full_url("AAAAAA", &h, Some("1"));
        let inv = parse_invite("devlocast", &url).expect("parse");
        assert_eq!(inv.host_pubkey, RFC8032_TEST1_PUBKEY);
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse_invite("locast", "").expect_err("empty must reject");
        assert!(matches!(err, InviteError::UrlParse(_)));
    }
}
