//! JWT-shaped (RFC 7519) access tokens, HS256 (HMAC-SHA256, RFC 4231)
//! signed, issued and verified with a single shared secret. This is what
//! `gems-webui` and `gems-mcp` use by default to establish who a network
//! caller is acting as — see their module docs — instead of trusting a
//! caller-supplied `subject`/`roles` argument at face value (which, before
//! this, let anyone claiming to be any subject actually read as that
//! subject: nothing verified the claim).
//!
//! Deliberately minimal relative to the full JWT spec: one fixed header
//! (`{"alg":"HS256","typ":"JWT"}`), one algorithm, three claims (`sub`,
//! `roles`, optional `exp`), no `aud`/`iss`/key-rotation/`kid` header.
//! Exactly what this workspace's own token issuance and verification need,
//! not a general-purpose JWT library — the token is only ever produced by
//! `issue` and only ever consumed by `verify`, both here, so there's no
//! interoperability requirement pulling in the rest of the spec.
//!
//! `verify` takes `now` as a parameter rather than reading the system
//! clock itself, so expiry logic is deterministically testable — the same
//! "pure core, caller supplies impure inputs" pattern used throughout this
//! workspace (`RaftCore::tick`, `SwimCore::tick`).

use gems_common::hmac::{constant_time_eq, hmac_sha256};
use gems_common::{base64url, Tuid};
use gems_json::Value;

use crate::SubjectContext;

const HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"; // {"alg":"HS256","typ":"JWT"}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Missing,
    Malformed,
    BadSignature,
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Missing => write!(f, "missing access token"),
            TokenError::Malformed => write!(f, "malformed token"),
            TokenError::BadSignature => write!(f, "invalid token signature"),
            TokenError::Expired => write!(f, "token has expired"),
        }
    }
}

impl std::error::Error for TokenError {}

/// A frontend's authentication posture, shared by `gems-webui` and
/// `gems-mcp` so the "require a verified bearer token by default, allow an
/// explicit `--insecure` opt-out" policy lives in one place rather than
/// being reimplemented per frontend.
#[derive(Clone)]
pub enum AuthMode {
    Enforced { secret: Vec<u8> },
    Insecure,
}

impl AuthMode {
    /// Authorizes a request given the bearer token it carried, if any (a
    /// frontend extracts this from wherever its transport puts it — an
    /// `Authorization` header for HTTP, a tool argument for MCP). Returns
    /// `Ok(None)` only under `Insecure`; under `Enforced`, a missing or
    /// invalid token is always `Err`, never a silent fall-through to raw
    /// access.
    pub fn authorize(
        &self,
        bearer_token: Option<&str>,
        now: u64,
    ) -> Result<Option<SubjectContext>, TokenError> {
        match self {
            AuthMode::Insecure => Ok(None),
            AuthMode::Enforced { secret } => {
                let token = bearer_token.ok_or(TokenError::Missing)?;
                verify(secret, token, now).map(Some)
            }
        }
    }
}

/// Issues a token asserting `subject`, optionally expiring at
/// `expires_at_unix_secs` (a Unix timestamp `verify` compares its own
/// `now` against; `None` means the token never expires).
pub fn issue(secret: &[u8], subject: &SubjectContext, expires_at_unix_secs: Option<u64>) -> String {
    let mut claims = Value::object();
    claims.set("sub", subject.subject_id.to_hex_string());
    let roles: Value = subject
        .roles
        .iter()
        .map(|r| r.to_hex_string())
        .collect::<Vec<_>>()
        .into();
    claims.set("roles", roles);
    if let Some(exp) = expires_at_unix_secs {
        claims.set("exp", exp);
    }

    let payload_b64 = base64url::encode(claims.to_string().as_bytes());
    let signing_input = format!("{HEADER_B64}.{payload_b64}");
    let signature = hmac_sha256(secret, signing_input.as_bytes());
    let signature_b64 = base64url::encode(&signature);
    format!("{signing_input}.{signature_b64}")
}

/// Verifies `token`'s signature against `secret` and, if it carries an
/// `exp` claim, that `now` (Unix seconds) hasn't passed it yet. Returns
/// the `SubjectContext` the token asserts on success.
pub fn verify(secret: &[u8], token: &str, now: u64) -> Result<SubjectContext, TokenError> {
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };
    if header_b64 != HEADER_B64 {
        // Only one algorithm/header is ever issued by `issue`, so any
        // other header is either a forgery attempt or a token this
        // workspace didn't produce — reject either way rather than trying
        // to support arbitrary `alg` values (the classic JWT "alg
        // confusion" foot-gun: never let the token itself pick how it's
        // verified).
        return Err(TokenError::Malformed);
    }

    let signing_input = format!("{header_b64}.{payload_b64}");
    let expected_signature = hmac_sha256(secret, signing_input.as_bytes());
    let actual_signature = base64url::decode(signature_b64).map_err(|_| TokenError::Malformed)?;
    if !constant_time_eq(&expected_signature, &actual_signature) {
        return Err(TokenError::BadSignature);
    }

    let payload_bytes = base64url::decode(payload_b64).map_err(|_| TokenError::Malformed)?;
    let payload_str = std::str::from_utf8(&payload_bytes).map_err(|_| TokenError::Malformed)?;
    let claims = gems_json::parse(payload_str).map_err(|_| TokenError::Malformed)?;

    let subject_id = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .and_then(Tuid::from_hex_str)
        .ok_or(TokenError::Malformed)?;
    let roles = claims
        .get("roles")
        .and_then(|v| v.as_array())
        .ok_or(TokenError::Malformed)?
        .iter()
        .map(|v| v.as_str().and_then(Tuid::from_hex_str))
        .collect::<Option<Vec<Tuid>>>()
        .ok_or(TokenError::Malformed)?;

    if let Some(exp) = claims.get("exp").and_then(|v| v.as_f64()) {
        if now as f64 >= exp {
            return Err(TokenError::Expired);
        }
    }

    Ok(SubjectContext { subject_id, roles })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(id_seed: u8, role_seeds: &[u8]) -> SubjectContext {
        SubjectContext {
            subject_id: Tuid::new([id_seed; 16], id_seed as u64),
            roles: role_seeds
                .iter()
                .map(|&r| Tuid::new([r; 16], r as u64))
                .collect(),
        }
    }

    #[test]
    fn issued_token_verifies_and_round_trips_the_subject() {
        let secret = b"test-secret";
        let subj = subject(1, &[2, 3]);
        let token = issue(secret, &subj, None);
        let verified = verify(secret, &token, 1000).unwrap();
        assert_eq!(verified.subject_id, subj.subject_id);
        assert_eq!(verified.roles, subj.roles);
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let subj = subject(1, &[]);
        let token = issue(b"correct-secret", &subj, None);
        let result = verify(b"wrong-secret", &token, 1000);
        assert_eq!(result.unwrap_err(), TokenError::BadSignature);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let secret = b"test-secret";
        let subj = subject(1, &[]);
        let token = issue(secret, &subj, None);
        let mut parts: Vec<&str> = token.split('.').collect();
        // Swap in a payload asserting a different subject, keeping the
        // original (now-mismatched) signature — the classic "edit the
        // claims, keep the old signature" forgery attempt.
        let forged_subject = subject(9, &[]);
        let forged_claims = {
            let mut c = Value::object();
            c.set("sub", forged_subject.subject_id.to_hex_string());
            c.set("roles", Value::array());
            c
        };
        let forged_payload_b64 =
            gems_common::base64url::encode(forged_claims.to_string().as_bytes());
        parts[1] = &forged_payload_b64;
        let forged_token = parts.join(".");
        assert_eq!(
            verify(secret, &forged_token, 1000).unwrap_err(),
            TokenError::BadSignature
        );
    }

    #[test]
    fn expired_token_is_rejected() {
        let secret = b"test-secret";
        let subj = subject(1, &[]);
        let token = issue(secret, &subj, Some(500));
        assert_eq!(
            verify(secret, &token, 500).unwrap_err(),
            TokenError::Expired
        );
        assert_eq!(
            verify(secret, &token, 1000).unwrap_err(),
            TokenError::Expired
        );
    }

    #[test]
    fn not_yet_expired_token_verifies() {
        let secret = b"test-secret";
        let subj = subject(1, &[]);
        let token = issue(secret, &subj, Some(500));
        assert!(verify(secret, &token, 100).is_ok());
    }

    #[test]
    fn malformed_tokens_are_rejected_without_panicking() {
        let secret = b"test-secret";
        for bad in ["", "not-a-token", "a.b", "a.b.c.d", "a.b."] {
            assert!(verify(secret, bad, 0).is_err());
        }
    }

    #[test]
    fn a_token_claiming_an_unsupported_algorithm_header_is_rejected() {
        // Guards the "alg confusion" class of JWT bug: verification must
        // never branch on what the token itself claims its algorithm is.
        let secret = b"test-secret";
        let fake_header = gems_common::base64url::encode(br#"{"alg":"none","typ":"JWT"}"#);
        let mut claims = Value::object();
        claims.set("sub", Tuid::new([1u8; 16], 1).to_hex_string());
        claims.set("roles", Value::array());
        let payload_b64 = gems_common::base64url::encode(claims.to_string().as_bytes());
        let forged = format!("{fake_header}.{payload_b64}.");
        assert_eq!(
            verify(secret, &forged, 0).unwrap_err(),
            TokenError::Malformed
        );
    }

    #[test]
    fn insecure_auth_mode_never_requires_a_token() {
        let mode = AuthMode::Insecure;
        assert_eq!(mode.authorize(None, 0), Ok(None));
        assert_eq!(mode.authorize(Some("garbage"), 0), Ok(None));
    }

    #[test]
    fn enforced_auth_mode_requires_a_token() {
        let mode = AuthMode::Enforced {
            secret: b"s".to_vec(),
        };
        assert_eq!(mode.authorize(None, 0), Err(TokenError::Missing));
    }

    #[test]
    fn enforced_auth_mode_verifies_a_present_token() {
        let secret = b"s".to_vec();
        let subj = subject(1, &[]);
        let token = issue(&secret, &subj, None);
        let mode = AuthMode::Enforced { secret };
        let verified = mode.authorize(Some(&token), 0).unwrap().unwrap();
        assert_eq!(verified.subject_id, subj.subject_id);
    }
}
