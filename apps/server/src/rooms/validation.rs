//! Display-name validation for room participants.
//!
//! P2-T05 tightens the v1 rule (was: `is_empty() || len() > 32`)
//! to also reject leading/trailing whitespace and any
//! C0 / C1 control character. The rules match the client-side
//! helper in `apps/client/src-tauri/src/identity/types.rs`
//! so the server cannot accept a name the client would not
//! have allowed.
//!
//! Note on NFC: the architecture calls for NFC-normalization
//! of display names. The server workspace does not currently
//! depend on `unicode-normalization`; rather than introduce a
//! new dependency, the validator here is intentionally
//! conservative. The character cap is measured in Unicode
//! scalar values (`chars().count()`), not bytes, and the
//! control-character check uses the full Unicode range
//! (`< 0x20` or `0x7F..=0x9F`). The canonical-equivalence
//! check (`e` + combining acute vs `e-acute`) is NOT
//! performed server-side. Clients are expected to submit
//! already-normalized text; P5+ may add the dep and a
//! normalization pass.

#![forbid(unsafe_code)]

use super::error::RoomError;

/// Maximum display-name length, in Unicode scalar values.
pub const MAX_DISPLAY_NAME_CHARS: usize = 32;

/// Trim leading/trailing ASCII whitespace and return the
/// remaining slice. Returns `None` if the result is empty.
fn trim_ascii(s: &str) -> Option<&str> {
    let trimmed = s.trim_matches(|c: char| {
        c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\x0B' || c == '\x0C'
    });
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Validate a user-supplied display name. The rules:
///
/// - Non-empty after ASCII whitespace trim.
/// - No leading or trailing whitespace.
/// - At most 32 Unicode scalar values.
/// - No C0 / C1 control characters.
pub fn validate_display_name(name: &str) -> Result<&str, RoomError> {
    if name.is_empty() {
        return Err(RoomError::InvalidState);
    }
    if name != name.trim() {
        return Err(RoomError::InvalidState);
    }
    // After the trim check above, leading/trailing
    // whitespace is gone. Belt-and-suspenders: if the trim
    // removed nothing, use the original; otherwise re-trim
    // and return the trimmed slice.
    let cleaned = match trim_ascii(name) {
        Some(s) => s,
        None => return Err(RoomError::InvalidState),
    };
    if cleaned.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(RoomError::InvalidState);
    }
    for c in cleaned.chars() {
        let cu = c as u32;
        if cu < 0x20 || (0x7F..=0x9F).contains(&cu) {
            return Err(RoomError::InvalidState);
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ascii_passes() {
        assert_eq!(validate_display_name("Alice").unwrap(), "Alice");
    }

    #[test]
    fn valid_unicode_passes() {
        // A name with non-ASCII characters (German sharp s
        // is in the BMP and is not a control char).
        assert_eq!(
            validate_display_name("Stra\u{00DF}e").unwrap(),
            "Stra\u{00DF}e"
        );
    }

    #[test]
    fn empty_rejected() {
        assert!(validate_display_name("").is_err());
    }

    #[test]
    fn whitespace_only_rejected() {
        assert!(validate_display_name("   ").is_err());
    }

    #[test]
    fn leading_or_trailing_whitespace_rejected() {
        assert!(validate_display_name(" Alice").is_err());
        assert!(validate_display_name("Alice ").is_err());
        assert!(validate_display_name("\tAlice\n").is_err());
    }

    #[test]
    fn exactly_32_chars_passes() {
        let s = "a".repeat(32);
        assert!(validate_display_name(&s).is_ok());
    }

    #[test]
    fn thirty_three_chars_rejected() {
        let s = "a".repeat(33);
        assert!(validate_display_name(&s).is_err());
    }

    #[test]
    fn control_char_rejected() {
        assert!(validate_display_name("Ali\x00ce").is_err());
        assert!(validate_display_name("Ali\x07ce").is_err());
        assert!(validate_display_name("Ali\x7Fce").is_err());
    }

    #[test]
    fn len_utf8_caps_at_32_scalar_values() {
        // A 32-char name where each char is 4 bytes UTF-8
        // is still 32 scalar values, and 128 bytes. The
        // byte cap (was: `name.len() > 32`) would have
        // rejected this; the new cap accepts it.
        let s = "\u{1F600}".repeat(32);
        assert!(validate_display_name(&s).is_ok());
    }
}
