//! Structured audit log writer (P8-T05).
//!
//! `docs/ARCHITECTURE.md` describes the audit log as structured JSON
//! events written to stdout and optionally to a file, with a redacted
//! payload: we never log bearer tokens, passwords, private key material,
//! TURN credentials, or signatures.
//!
//! [`AuditWriter`] is the only way to emit an audit event, and it always
//! runs [`redact`] on the payload first, so callers cannot forget to.
//! Redaction is by field name: any object key that names one of
//! [`REDACTED_FIELDS`] is removed, at any depth.

use std::io::{self, Write};

use serde_json::{json, Map, Value};

/// Field names that are stripped from every audit payload. Matching
/// ignores case and separators and also catches longer names that
/// contain one of these (`bearer_token`, `turnCredential`,
/// `private-key-pem`, `signatures`).
pub const REDACTED_FIELDS: &[&str] = &[
    "bearer",
    "password",
    "private_key",
    "credential",
    "signature",
];

/// Lowercase ASCII alphanumerics only, so `privateKey`, `private_key`
/// and `PRIVATE-KEY` all compare equal.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// True if an object key must be stripped from an audit payload.
pub fn is_redacted_field(key: &str) -> bool {
    let key = normalize(key);
    REDACTED_FIELDS
        .iter()
        .any(|field| key.contains(normalize(field).as_str()))
}

/// Remove every redacted field from `value`, recursing into nested
/// objects and arrays.
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => redact_map(map),
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

fn redact_map(map: &mut Map<String, Value>) {
    map.retain(|key, _| !is_redacted_field(key));
    map.values_mut().for_each(redact);
}

/// Writes one JSON object per line: `{"ts_ms", "type", "payload"}`.
/// The payload is always redacted before it is serialized.
pub struct AuditWriter<W: Write> {
    out: W,
}

impl AuditWriter<io::Stdout> {
    /// Audit writer on stdout, the default sink.
    pub fn stdout() -> Self {
        Self::new(io::stdout())
    }
}

impl<W: Write> AuditWriter<W> {
    /// Audit writer on any sink (for example an append-mode file).
    pub fn new(out: W) -> Self {
        Self { out }
    }

    /// Redact `payload` and append one event line.
    pub fn write_event(
        &mut self,
        ts_ms: i64,
        event_type: &str,
        mut payload: Value,
    ) -> io::Result<()> {
        redact(&mut payload);
        let event = json!({ "ts_ms": ts_ms, "type": event_type, "payload": payload });
        // Serialize first, then emit with a single `write_all`, so a
        // line cannot interleave with other output on a shared stdout.
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        self.out.write_all(&line)?;
        self.out.flush()
    }

    /// Recover the sink (used by tests to inspect what was written).
    pub fn into_inner(self) -> W {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct, unusual values so a leak is unambiguous.
    const BEARER: &str = "brr-5f1c9a0e7d";
    const PASSWORD: &str = "pw-Hk3!q9z";
    const PRIVATE_KEY: &str = "pk-MC4CAQAwBQYDK2VwBCIEI";
    const CREDENTIAL: &str = "cred-1730000000-hmac-b64";
    const SIGNATURE: &str = "sig-ed25519-AbCdEf012";
    const SECRETS: &[&str] = &[BEARER, PASSWORD, PRIVATE_KEY, CREDENTIAL, SIGNATURE];

    fn write(payload: Value) -> String {
        let mut w = AuditWriter::new(Vec::new());
        w.write_event(1_700_000_000_000, "auth.ok", payload)
            .expect("write");
        String::from_utf8(w.into_inner()).expect("utf8")
    }

    fn assert_no_secrets(line: &str) {
        for s in SECRETS {
            assert!(
                !line.contains(s),
                "secret {s:?} leaked into audit line: {line}"
            );
        }
    }

    #[test]
    fn writer_strips_the_five_named_fields() {
        let line = write(json!({
            "bearer": BEARER,
            "password": PASSWORD,
            "private_key": PRIVATE_KEY,
            "credential": CREDENTIAL,
            "signature": SIGNATURE,
            "user_id": "u-123",
            "room_code": "ABC234",
        }));
        assert_no_secrets(&line);
        let v: Value = serde_json::from_str(line.trim_end()).expect("json");
        let payload = v["payload"].as_object().expect("object");
        for field in REDACTED_FIELDS {
            assert!(!payload.contains_key(*field), "{field} not stripped");
        }
        // Safe context survives.
        assert_eq!(payload["user_id"], "u-123");
        assert_eq!(payload["room_code"], "ABC234");
        assert_eq!(v["type"], "auth.ok");
        assert_eq!(v["ts_ms"], 1_700_000_000_000_i64);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn nested_and_array_fields_are_stripped() {
        let line = write(json!({
            "turn": { "username": "1730000000:u-1", "credential": CREDENTIAL },
            "peers": [ { "id": "p1", "signature": SIGNATURE }, { "id": "p2", "bearer": BEARER } ],
            "auth": { "inner": { "password": PASSWORD, "private_key": PRIVATE_KEY } },
        }));
        assert_no_secrets(&line);
        let v: Value = serde_json::from_str(line.trim_end()).expect("json");
        assert_eq!(v["payload"]["turn"]["username"], "1730000000:u-1");
        assert_eq!(v["payload"]["peers"][1]["id"], "p2");
    }

    #[test]
    fn name_variants_are_stripped() {
        let line = write(json!({
            "Bearer": BEARER,
            "bearer_token": BEARER,
            "PASSWORD": PASSWORD,
            "privateKey": PRIVATE_KEY,
            "private-key-pem": PRIVATE_KEY,
            "turn_credential": CREDENTIAL,
            "credentials": CREDENTIAL,
            "signatures": [SIGNATURE],
            "kept": "ok",
        }));
        assert_no_secrets(&line);
        let v: Value = serde_json::from_str(line.trim_end()).expect("json");
        assert_eq!(v["payload"], json!({ "kept": "ok" }));
    }

    #[test]
    fn non_object_payloads_pass_through() {
        assert_eq!(
            serde_json::from_str::<Value>(write(json!("plain")).trim_end()).expect("json")
                ["payload"],
            "plain"
        );
        assert_eq!(
            serde_json::from_str::<Value>(write(Value::Null).trim_end()).expect("json")["payload"],
            Value::Null
        );
    }

    #[test]
    fn unrelated_field_names_are_kept() {
        for key in ["user_id", "room", "reason", "codec", "display_name"] {
            assert!(!is_redacted_field(key), "{key} should be kept");
        }
    }

    #[test]
    fn matching_fails_closed_on_names_containing_a_redacted_word() {
        // A name that merely contains a redacted word is stripped too;
        // losing a harmless field beats leaking a secret.
        assert!(is_redacted_field("bearer_ttl"));
    }
}
