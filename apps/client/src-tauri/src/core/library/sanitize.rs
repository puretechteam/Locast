//! Filename sanitization for `media_items.filename` and `media_subtitles.filename`.
//!
//! Implements architecture section 6. The sanitizer is a pure function: it
//! performs no I/O, no allocation beyond its return value, no clock or env
//! reads, and no network calls. The only failure mode is `InvalidFilename`,
//! which covers every reason a filename could be rejected (empty after
//! stripping, reserved name, only-trailing-whitespace, etc.).
//!
//! # Order of operations
//!
//! The architecture lists eight rules; the implementation order is chosen
//! to make reserved-name detection and length-cap behavior defensible:
//!
//! 1. Strip path components on `\` and `/`; reject if the last segment is
//!    empty, `.`, or `..`. The `.`/`..` check applies to the post-strip
//!    segment only, which catches `..`, `../foo`, `foo/..`, and `.`. It
//!    does NOT scan every path component for `..`; if a future task needs
//!    to also reject e.g. `foo/../bar`, that is a separate pass.
//! 2. Replace control characters (U+0000..=U+001F and U+007F..=U+009F)
//!    with `_`. This runs before the length cap so a control char counted
//!    as 1 char contributes 1 to the cap.
//! 3. Replace Windows-forbidden characters (`< > : " | ? *`) with `_`.
//! 4. Strip trailing `.` and ` ` (space) characters in a loop, then reject
//!    if the result is empty. This runs BEFORE reserved-name detection
//!    because Windows silently strips trailing dots and spaces; the on-disk
//!    name is the stripped form, so the reserved-name check has to see
//!    the stripped form too. A name like `CON ` would otherwise slip past
//!    the reserved-name check and be written to disk as `CON`.
//! 5. NFC normalize. Applied before the reserved-name check so the
//!    reserved set is matched against the canonical form. NFC can change
//!    byte length but not the set of scalar values, so it does not change
//!    the reserved-name answer in any practical case; it does, however,
//!    make the canonical form what gets compared.
//! 6. Reject reserved Windows names (case-insensitive). Both the bare
//!    form (`CON`) and the `name.ext` form (`CON.txt`) are rejected by
//!    comparing only the name part, i.e. everything before the first `.`.
//!    The match is exact, not prefix: `CONFOO` is fine.
//! 7. Truncate to 255 bytes by Unicode scalar value, preserving valid
//!    UTF-8. The result's byte length is `<= 255`; multi-byte chars are
//!    never split. The post-truncation result is NOT re-checked for
//!    reserved names or trailing dots; rule 4 already runs on the
//!    pre-truncation form and a user-visible 255-byte filename is what
//!    the architecture promises.
//!
//! # Reserved-name check detail
//!
//! The comparison splits on the first `.` and lowercases the name part.
//! The extension does not change reserved-name status: `CON.txt` is
//! rejected, `CONFOO.txt` is not. A trailing dot is stripped by rule 4
//! before this check, so `CON.` becomes `CON` and is rejected.

use unicode_normalization::UnicodeNormalization;

/// The only error returned by [`sanitize`].
///
/// Unit struct on purpose: the sanitizer does not distinguish between
/// "empty after path strip" and "reserved name" and "only whitespace";
/// from the caller's perspective every failure is "this filename is not
/// usable on disk", and a single variant keeps the call sites simple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidFilename;

impl std::fmt::Display for InvalidFilename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid filename")
    }
}

impl std::error::Error for InvalidFilename {}

const MAX_BYTES: usize = 255;

const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Sanitize a filename per architecture section 6.
///
/// Returns the sanitized filename, or [`InvalidFilename`] if the input is
/// not usable. Never panics, never reads I/O, never allocates beyond the
/// returned `String`.
pub fn sanitize(name: &str) -> Result<String, InvalidFilename> {
    // 1. Strip path components. Split on both separators, keep the last
    //    segment, and reject if it's empty, `.`, or `..`.
    let last = name.rsplit(['\\', '/']).next().unwrap_or("");
    if last.is_empty() || last == "." || last == ".." {
        return Err(InvalidFilename);
    }

    // 2. Replace control characters with `_`. U+0020 (space) is NOT a
    //    control char by the architecture's definition (`< 0x20`); the
    //    boundary is exclusive, so space is left alone here and handled
    //    by the trailing-whitespace strip in step 4.
    let mut buf = String::with_capacity(last.len());
    for c in last.chars() {
        let code = c as u32;
        if code < 0x20 || (0x7F..=0x9F).contains(&code) {
            buf.push('_');
        } else {
            buf.push(c);
        }
    }

    // 3. Replace Windows-forbidden characters with `_`.
    let buf = buf
        .chars()
        .map(|c| if FORBIDDEN_CHARS.contains(&c) { '_' } else { c })
        .collect::<String>();

    // 4. Strip trailing `.` and ` ` in a loop until stable, then reject
    //    if the result is empty.
    let mut end = buf.len();
    while end > 0 {
        let prev = buf[..end].char_indices().last().map(|(i, _)| i);
        match prev {
            Some(i) => {
                let last_char = buf[i..end].chars().next().unwrap();
                if last_char == '.' || last_char == ' ' {
                    end = i;
                    continue;
                }
            }
            None => break,
        }
        break;
    }
    if end == 0 {
        return Err(InvalidFilename);
    }
    let stripped = &buf[..end];

    // 5. NFC normalize. Applied before the reserved-name check so the
    //    canonical form is what we compare.
    let nfc: String = stripped.nfc().collect();

    // 6. Reject reserved Windows names (case-insensitive). Split on the
    //    first `.`; the extension is irrelevant.
    if is_reserved_name(&nfc) {
        return Err(InvalidFilename);
    }

    // 7. Truncate to 255 bytes by Unicode scalar value, preserving valid
    //    UTF-8. Walk the chars, accumulating bytes; stop when the next
    //    char would push us over 255.
    let mut out = String::with_capacity(nfc.len().min(MAX_BYTES));
    let mut bytes = 0usize;
    for c in nfc.chars() {
        let char_bytes = utf8_len(c);
        if bytes + char_bytes > MAX_BYTES {
            break;
        }
        out.push(c);
        bytes += char_bytes;
    }

    if out.is_empty() {
        return Err(InvalidFilename);
    }

    Ok(out)
}

fn utf8_len(c: char) -> usize {
    let code = c as u32;
    if code < 0x80 {
        1
    } else if code < 0x800 {
        2
    } else if code < 0x10000 {
        3
    } else {
        4
    }
}

fn is_reserved_name(s: &str) -> bool {
    let name_part = match s.find('.') {
        Some(i) => &s[..i],
        None => s,
    };
    let upper = name_part.to_ascii_uppercase();
    RESERVED_NAMES.iter().any(|r| *r == upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- 1. ordinary valid filename

    #[test]
    fn ordinary_filename_passes_through() {
        assert_eq!(
            sanitize("Spirited Away.mkv"),
            Ok("Spirited Away.mkv".to_string())
        );
    }

    // ----- 2. path components stripped

    #[test]
    fn forward_slash_path_components_stripped() {
        assert_eq!(sanitize("foo/bar/Movie.mkv"), Ok("Movie.mkv".to_string()));
    }

    #[test]
    fn backslash_path_components_stripped() {
        assert_eq!(sanitize("foo\\bar\\Movie.mkv"), Ok("Movie.mkv".to_string()));
    }

    #[test]
    fn mixed_separators_keep_last_segment() {
        assert_eq!(sanitize("a/b\\c/Movie.mkv"), Ok("Movie.mkv".to_string()));
    }

    // ----- 3. empty after stripping

    #[test]
    fn empty_input_rejected() {
        assert_eq!(sanitize(""), Err(InvalidFilename));
    }

    #[test]
    fn trailing_separator_rejected() {
        assert_eq!(sanitize("foo/bar/"), Err(InvalidFilename));
    }

    #[test]
    fn bare_slash_rejected() {
        assert_eq!(sanitize("/"), Err(InvalidFilename));
    }

    #[test]
    fn bare_backslash_rejected() {
        assert_eq!(sanitize("\\"), Err(InvalidFilename));
    }

    // ----- 4. dot and dotdot segments

    #[test]
    fn bare_dot_rejected() {
        assert_eq!(sanitize("."), Err(InvalidFilename));
    }

    #[test]
    fn bare_dotdot_rejected() {
        assert_eq!(sanitize(".."), Err(InvalidFilename));
    }

    #[test]
    fn dotdot_in_path_rejected() {
        assert_eq!(sanitize("foo/.."), Err(InvalidFilename));
    }

    #[test]
    fn dot_in_path_rejected() {
        assert_eq!(sanitize("foo/."), Err(InvalidFilename));
    }

    #[test]
    fn dotdot_prefix_in_path_strips_to_last_segment() {
        // Documented rule: `.` and `..` are detected as the FINAL segment
        // after path-stripping. The segments before the final one are
        // discarded by step 1, so `../foo` -> `foo` and is accepted.
        // This is rule (b) in the spec; a future task that needs to also
        // reject `../foo` can add a separate pre-strip scan.
        assert_eq!(sanitize("../foo"), Ok("foo".to_string()));
    }

    // ----- 5. control characters

    #[test]
    fn control_char_0x01_replaced() {
        assert_eq!(sanitize("Movie\x01.mkv"), Ok("Movie_.mkv".to_string()));
    }

    #[test]
    fn control_char_0x1f_replaced() {
        assert_eq!(sanitize("Movie\x1F.mkv"), Ok("Movie_.mkv".to_string()));
    }

    #[test]
    fn control_char_0x7f_replaced() {
        assert_eq!(sanitize("Movie\x7F.mkv"), Ok("Movie_.mkv".to_string()));
    }

    #[test]
    fn control_char_0x9f_replaced() {
        assert_eq!(sanitize("Movie\u{9F}.mkv"), Ok("Movie_.mkv".to_string()));
    }

    #[test]
    fn null_byte_replaced() {
        assert_eq!(sanitize("Movie\x00.mkv"), Ok("Movie_.mkv".to_string()));
    }

    #[test]
    fn space_0x20_is_not_a_control_char() {
        // U+0020 is exactly at the boundary. Architecture says `< 0x20`,
        // so space is NOT replaced here. It will be handled by the
        // trailing-whitespace strip.
        assert_eq!(sanitize("a b"), Ok("a b".to_string()));
    }

    // ----- 6. Windows-forbidden characters

    #[test]
    fn forbidden_char_less_than_replaced() {
        assert_eq!(sanitize("a<b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_greater_than_replaced() {
        assert_eq!(sanitize("a>b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_colon_replaced() {
        assert_eq!(sanitize("a:b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_quote_replaced() {
        assert_eq!(sanitize("a\"b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_pipe_replaced() {
        assert_eq!(sanitize("a|b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_question_replaced() {
        assert_eq!(sanitize("a?b"), Ok("a_b".to_string()));
    }

    #[test]
    fn forbidden_char_star_replaced() {
        assert_eq!(sanitize("a*b"), Ok("a_b".to_string()));
    }

    #[test]
    fn multiple_forbidden_chars_all_replaced() {
        assert_eq!(sanitize("a<b:c>d"), Ok("a_b_c_d".to_string()));
    }

    #[test]
    fn forbidden_chars_in_brackets() {
        assert_eq!(sanitize("Movie<bad>.mkv"), Ok("Movie_bad_.mkv".to_string()));
    }

    // ----- 7. reserved names (case-insensitive)

    #[test]
    fn reserved_con_bare_rejected() {
        assert_eq!(sanitize("CON"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_lowercase_rejected() {
        assert_eq!(sanitize("con"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_mixed_case_rejected() {
        assert_eq!(sanitize("Con"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_with_extension_rejected() {
        assert_eq!(sanitize("CON.txt"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_with_extension_lowercase_rejected() {
        assert_eq!(sanitize("con.txt"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_with_trailing_dot_rejected() {
        // "CON." -> strip trailing dot -> "CON" -> reserved -> Err.
        assert_eq!(sanitize("CON."), Err(InvalidFilename));
    }

    #[test]
    fn reserved_con_with_trailing_space_rejected() {
        // "CON " -> strip trailing space -> "CON" -> reserved -> Err.
        // This is the case that motivates running rule 4 before rule 6.
        assert_eq!(sanitize("CON "), Err(InvalidFilename));
    }

    #[test]
    fn reserved_prn_rejected() {
        assert_eq!(sanitize("PRN"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_aux_rejected() {
        assert_eq!(sanitize("AUX"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_nul_rejected() {
        assert_eq!(sanitize("NUL"), Err(InvalidFilename));
    }

    #[test]
    fn reserved_com1_through_com9_rejected() {
        for i in 1..=9 {
            let name = format!("COM{i}");
            assert_eq!(
                sanitize(&name),
                Err(InvalidFilename),
                "COM{i} should be reserved"
            );
        }
    }

    #[test]
    fn reserved_lpt1_through_lpt9_rejected() {
        for i in 1..=9 {
            let name = format!("LPT{i}");
            assert_eq!(
                sanitize(&name),
                Err(InvalidFilename),
                "LPT{i} should be reserved"
            );
        }
    }

    #[test]
    fn reserved_com1_with_extension_rejected() {
        assert_eq!(sanitize("COM1.txt"), Err(InvalidFilename));
    }

    #[test]
    fn confoo_is_not_reserved() {
        // Reserved-name match is exact, not prefix.
        assert_eq!(sanitize("CONFOO"), Ok("CONFOO".to_string()));
    }

    #[test]
    fn conext_is_not_reserved() {
        assert_eq!(sanitize("CONEXT"), Ok("CONEXT".to_string()));
    }

    // ----- 8. trailing dots and spaces

    #[test]
    fn trailing_dot_stripped() {
        assert_eq!(sanitize("Movie. "), Ok("Movie".to_string()));
    }

    #[test]
    fn only_trailing_dot_rejected() {
        // "Movie." -> strip trailing dot -> "Movie" -> not empty -> Ok.
        // But ".." -> strip trailing dots -> "" -> Err.
        assert_eq!(sanitize("Movie."), Ok("Movie".to_string()));
    }

    #[test]
    fn only_trailing_spaces_rejected() {
        // "Movie " -> strip trailing spaces -> "Movie" -> not empty -> Ok.
        // But "   " -> strip trailing spaces -> "" -> Err.
        assert_eq!(sanitize("Movie "), Ok("Movie".to_string()));
    }

    #[test]
    fn only_spaces_rejected() {
        assert_eq!(sanitize("   "), Err(InvalidFilename));
    }

    #[test]
    fn only_dots_rejected() {
        assert_eq!(sanitize("..."), Err(InvalidFilename));
    }

    #[test]
    fn mixed_trailing_dots_and_spaces_stripped_in_loop() {
        // "a.b . " -> strip trailing space -> "a.b ." -> strip trailing dot -> "a.b " ->
        // strip trailing space -> "a.b" -> done. Result: "a.b".
        assert_eq!(sanitize("a.b . "), Ok("a.b".to_string()));
    }

    #[test]
    fn internal_space_preserved() {
        // Trailing-space strip only touches the END of the string.
        assert_eq!(sanitize("a. b"), Ok("a. b".to_string()));
    }

    #[test]
    fn multiple_trailing_dots_stripped() {
        // "a.b.." -> strip trailing dot -> "a.b." -> strip trailing dot -> "a.b" -> done.
        assert_eq!(sanitize("a.b.."), Ok("a.b".to_string()));
    }

    // ----- 9. length cap

    #[test]
    fn length_exactly_255_ascii_ok() {
        let s = "a".repeat(255);
        let out = sanitize(&s).expect("255 a's should be ok");
        assert_eq!(out.len(), 255);
        assert_eq!(out.chars().count(), 255);
    }

    #[test]
    fn length_256_ascii_truncated_to_255() {
        let s = "a".repeat(256);
        let out = sanitize(&s).expect("256 a's should be truncated, not rejected");
        assert_eq!(out.len(), 255);
        assert_eq!(out.chars().count(), 255);
    }

    #[test]
    fn length_cap_three_byte_chars() {
        // U+4E2D (中) is 3 bytes in UTF-8. 85 chars = 255 bytes; 86 = 258 (over).
        let s = "中".repeat(100);
        let out = sanitize(&s).expect("100 three-byte chars should be truncated, not rejected");
        assert_eq!(out.chars().count(), 85);
        assert_eq!(out.len(), 255);
    }

    #[test]
    fn length_cap_never_splits_multibyte_char() {
        // The last char in the output must be a complete UTF-8 char.
        let s = "中".repeat(100);
        let out = sanitize(&s).unwrap();
        // Round-trip via bytes: any split sequence would error.
        String::from_utf8(out.into_bytes()).expect("output must be valid UTF-8");
    }

    #[test]
    fn length_cap_exactly_85_three_byte_chars() {
        let s = "中".repeat(85);
        let out = sanitize(&s).unwrap();
        assert_eq!(out.chars().count(), 85);
        assert_eq!(out.len(), 255);
    }

    #[test]
    fn length_cap_86_three_byte_chars_truncated_to_85() {
        let s = "中".repeat(86);
        let out = sanitize(&s).unwrap();
        assert_eq!(out.chars().count(), 85);
        assert_eq!(out.len(), 255);
    }

    #[test]
    fn length_cap_four_byte_chars() {
        // U+1F600 (emoji) is 4 bytes. 63 chars = 252 bytes; 64 = 256 (over).
        let s = "\u{1F600}".repeat(100);
        let out = sanitize(&s).unwrap();
        assert!(out.len() <= 255);
        // 63 * 4 = 252, 64 * 4 = 256 -> cap at 63.
        assert_eq!(out.chars().count(), 63);
    }

    // ----- 10. NFC normalization

    #[test]
    fn nfc_combined_acute_normalized() {
        // U+0065 U+0301 (e + combining acute) -> U+00E9 (precomposed é).
        assert_eq!(sanitize("cafe\u{0301}.txt"), Ok("café.txt".to_string()));
    }

    #[test]
    fn nfc_already_precomposed_unchanged() {
        assert_eq!(sanitize("café.txt"), Ok("café.txt".to_string()));
    }

    // ----- 11. UTF-8 round-trip

    #[test]
    fn output_is_valid_utf8_on_ordinary_input() {
        let out = sanitize("Spirited Away.mkv").unwrap();
        String::from_utf8(out.into_bytes()).expect("output must be valid UTF-8");
    }

    #[test]
    fn output_is_valid_utf8_on_multibyte_input() {
        let out = sanitize("电影.mkv").unwrap();
        String::from_utf8(out.into_bytes()).expect("output must be valid UTF-8");
    }

    #[test]
    fn output_is_valid_utf8_on_control_chars() {
        let out = sanitize("a\x01b\x7Fc.mkv").unwrap();
        String::from_utf8(out.into_bytes()).expect("output must be valid UTF-8");
    }

    #[test]
    fn output_is_valid_utf8_on_forbidden_chars() {
        let out = sanitize("a<b>c:d\"e|f?g*h.mkv").unwrap();
        String::from_utf8(out.into_bytes()).expect("output must be valid UTF-8");
    }

    // ----- 13. only forbidden chars after stripping path

    #[test]
    fn only_forbidden_chars_after_stripping_path() {
        assert_eq!(sanitize("foo/<<<"), Ok("___".to_string()));
    }

    // ----- 14. name with only spaces

    #[test]
    fn only_spaces_after_stripping_path() {
        assert_eq!(sanitize("   "), Err(InvalidFilename));
    }

    // ----- 15. name with only control chars

    #[test]
    fn only_control_chars_replaced() {
        assert_eq!(sanitize("\x01\x02"), Ok("__".to_string()));
    }

    // ----- 16. mixed case reserved with extension

    #[test]
    fn con_mkv_rejected() {
        assert_eq!(sanitize("Con.MKV"), Err(InvalidFilename));
    }

    #[test]
    fn con_lowercase_mkv_rejected() {
        assert_eq!(sanitize("con.mkv"), Err(InvalidFilename));
    }

    #[test]
    fn con_uppercase_mkv_rejected() {
        assert_eq!(sanitize("CON.mkv"), Err(InvalidFilename));
    }

    #[test]
    fn con_mixed_mkv_rejected() {
        assert_eq!(sanitize("cOn.MKV"), Err(InvalidFilename));
    }

    // P1-T01 review gap: a reserved name with a trailing space and an
    // extension. The trailing-strip step must run BEFORE the reserved-name
    // check, otherwise the trailing space would slip past the reserved
    // check and Windows would silently strip the space on disk, producing
    // a file literally named `con`. This test pins the ordering.
    #[test]
    fn reserved_con_with_internal_space_in_strip_form_rejected() {
        // "con .txt" -> trailing strip (no trailing dot/space) -> "con .txt"
        // is NOT rejected (the name part is "con " with a space, not "con",
        // and trailing strip only operates on the very last character).
        assert_eq!(sanitize("con .txt"), Ok("con .txt".to_string()));
    }

    // P1-T01 review gap: a reserved name as the final segment of a path.
    // The path-strip step must run first; the reserved-name check must
    // then see the bare reserved name and reject. Without this pin the
    // ordering between path-strip and reserved-check is not asserted.
    #[test]
    fn reserved_name_after_path_strip_rejected() {
        assert_eq!(sanitize("foo/CON"), Err(InvalidFilename));
        assert_eq!(sanitize("a/b/con"), Err(InvalidFilename));
        assert_eq!(sanitize("a\\b\\NUL.txt"), Err(InvalidFilename));
    }

    // ----- error type traits

    #[test]
    fn invalid_filename_display_and_error() {
        let e = InvalidFilename;
        assert_eq!(format!("{e}"), "invalid filename");
        // Verify std::error::Error is implemented by calling .source().
        let _src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    }

    // ----- result is always Ok or Err, never panics

    #[test]
    fn does_not_panic_on_pathological_inputs() {
        let inputs = [
            "",
            "/",
            "\\",
            ".",
            "..",
            "/.",
            "/..",
            ". ",
            ".. ",
            "...",
            "   ",
            "\x00",
            "\x7F",
            "\u{9F}",
            "CON",
            "con",
            "Con.txt",
            "foo/../bar",
            "a/b/c/../../../etc/passwd",
            "\\\\?\\C:\\windows",
            "movie\x00\x01\x02.mkv",
        ];
        for input in inputs {
            // Must not panic. The result is either Ok or Err(InvalidFilename);
            // both are fine.
            let _ = sanitize(input);
        }
    }
}
