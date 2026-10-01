//! `media::subtitles::error` - the subtitle parse error type
//! (P8-T03, architecture sections 17.3 and 21.8).
//!
//! Payloads are plain-old-data only: line numbers, limits, and
//! `&'static str` labels. An error never carries offending text or
//! paths, so it is safe to log and to show verbatim.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

/// Why a subtitle file was rejected. Any hard cap rejects the whole
/// file; parsers never return a truncated cue list.
///
/// Line numbers are 1-based and refer to the decoded text (after
/// BOM handling and line splitting), saturating at `u32::MAX`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SubtitleError {
    /// The input is larger than the input cap (`limit` bytes).
    #[error("subtitle: input larger than {limit} bytes")]
    TooLarge {
        /// The cap that was exceeded.
        limit: u64,
    },
    /// The input is empty, only a BOM, or only whitespace.
    #[error("subtitle: empty input")]
    EmptyInput,
    /// The input is not valid UTF-8 or UTF-16 (strict decode), or it
    /// contains a NUL character.
    #[error("subtitle: invalid text encoding")]
    InvalidEncoding,
    /// The format's required header or section is missing.
    #[error("subtitle: missing header")]
    MissingHeader,
    /// A timestamp could not be parsed.
    #[error("subtitle: bad timestamp at line {line}")]
    BadTimestamp {
        /// 1-based line number.
        line: u32,
    },
    /// A cue ends before it starts (equal start and end is allowed).
    #[error("subtitle: end before start at line {line}")]
    TimestampOrder {
        /// 1-based line number.
        line: u32,
    },
    /// A timestamp is above the 100 hour cap or overflowed.
    #[error("subtitle: timestamp too large at line {line}")]
    TimestampTooLarge {
        /// 1-based line number.
        line: u32,
    },
    /// The file has more lines than the line cap.
    #[error("subtitle: too many lines")]
    TooManyLines,
    /// A line is longer than the per-line cap.
    #[error("subtitle: line too long at line {line}")]
    LineTooLong {
        /// 1-based line number.
        line: u32,
    },
    /// The file holds more cues than the cue cap (`limit`).
    #[error("subtitle: more than {limit} cues")]
    TooManyCues {
        /// The cap that was exceeded.
        limit: u32,
    },
    /// One cue's text is longer than the per-cue cap.
    #[error("subtitle: cue text too long at line {line}")]
    CueTooLong {
        /// 1-based line number.
        line: u32,
    },
    /// The input is non-empty but yielded no usable cues.
    #[error("subtitle: no cues")]
    NoCues,
    /// A structural problem specific to one format.
    #[error("subtitle: malformed {what} at line {line}")]
    Malformed {
        /// 1-based line number.
        line: u32,
        /// A fixed label naming the problem (never file content).
        what: &'static str,
    },
}

impl SubtitleError {
    /// A stable snake_case code for this error class, suitable for
    /// logs and (later) IPC. Never contains file content.
    pub fn code(&self) -> &'static str {
        match self {
            SubtitleError::TooLarge { .. } => "too_large",
            SubtitleError::EmptyInput => "empty_input",
            SubtitleError::InvalidEncoding => "invalid_encoding",
            SubtitleError::MissingHeader => "missing_header",
            SubtitleError::BadTimestamp { .. } => "bad_timestamp",
            SubtitleError::TimestampOrder { .. } => "timestamp_order",
            SubtitleError::TimestampTooLarge { .. } => "timestamp_too_large",
            SubtitleError::TooManyLines => "too_many_lines",
            SubtitleError::LineTooLong { .. } => "line_too_long",
            SubtitleError::TooManyCues { .. } => "too_many_cues",
            SubtitleError::CueTooLong { .. } => "cue_too_long",
            SubtitleError::NoCues => "no_cues",
            SubtitleError::Malformed { .. } => "malformed",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn all_variants() -> Vec<SubtitleError> {
        vec![
            SubtitleError::TooLarge { limit: 16 },
            SubtitleError::EmptyInput,
            SubtitleError::InvalidEncoding,
            SubtitleError::MissingHeader,
            SubtitleError::BadTimestamp { line: 12 },
            SubtitleError::TimestampOrder { line: 3 },
            SubtitleError::TimestampTooLarge { line: 4 },
            SubtitleError::TooManyLines,
            SubtitleError::LineTooLong { line: 5 },
            SubtitleError::TooManyCues { limit: 7 },
            SubtitleError::CueTooLong { line: 6 },
            SubtitleError::NoCues,
            SubtitleError::Malformed {
                line: 9,
                what: "dialogue before format",
            },
        ]
    }

    #[test]
    fn display_is_a_fixed_ascii_string_with_pod_payloads() {
        assert_eq!(
            SubtitleError::BadTimestamp { line: 12 }.to_string(),
            "subtitle: bad timestamp at line 12"
        );
        assert_eq!(
            SubtitleError::TooLarge { limit: 16 }.to_string(),
            "subtitle: input larger than 16 bytes"
        );
        assert_eq!(
            SubtitleError::Malformed {
                line: 9,
                what: "text column"
            }
            .to_string(),
            "subtitle: malformed text column at line 9"
        );
        for e in all_variants() {
            let s = e.to_string();
            assert!(s.starts_with("subtitle: "), "{s}");
            assert!(s.is_ascii(), "{s}");
        }
    }

    #[test]
    fn codes_are_unique_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for e in all_variants() {
            let c = e.code();
            assert!(
                c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{c}"
            );
            assert!(seen.insert(c), "duplicate code {c}");
        }
        assert_eq!(seen.len(), 13);
    }

    #[test]
    fn specific_codes_match_the_documented_names() {
        assert_eq!(SubtitleError::NoCues.code(), "no_cues");
        assert_eq!(
            SubtitleError::BadTimestamp { line: 1 }.code(),
            "bad_timestamp"
        );
        assert_eq!(SubtitleError::TooLarge { limit: 1 }.code(), "too_large");
    }

    #[test]
    fn errors_compare_and_clone() {
        let a = SubtitleError::LineTooLong { line: 2 };
        assert_eq!(a.clone(), a);
        assert_ne!(a, SubtitleError::LineTooLong { line: 3 });
    }
}
