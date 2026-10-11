//! Caller identity: a customer, or the operator.
//!
//! There is no account system. A customer's credential *is* the prefix uuid
//! they are working with (a capability). The operator proves itself with a
//! bearer token supplied via `ADMIN_TOKEN`.
//!
//! Nothing here rejects a request: an absent or wrong token simply resolves to
//! [`Role::Customer`]. Handlers decide what that means for them, which keeps
//! "customer may also do this" rules (delete a writable prefix, write cards
//! while not frozen) in one obvious place per endpoint.

use axum::http::{header, HeaderMap};

use crate::error::ApiError;
use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Anyone who knows a prefix uuid.
    Customer,
    /// The operator, holding the admin bearer token.
    Admin,
}

#[derive(Debug, Clone, Copy)]
pub struct Auth {
    pub role: Role,
    /// Whether an admin token is configured at all.
    configured: bool,
}

impl Auth {
    pub fn is_admin(self) -> bool {
        self.role == Role::Admin
    }

    /// Guard for admin-only endpoints.
    ///
    /// Returns 503 — not 401 — when no token is configured at all, so an
    /// unconfigured deployment reads as "admin API is switched off" rather
    /// than looking like a rejected password.
    pub fn require_admin(self) -> Result<(), ApiError> {
        if !self.configured {
            return Err(ApiError::Unavailable(
                "admin API is disabled: ADMIN_TOKEN is not set".into(),
            ));
        }
        if !self.is_admin() {
            return Err(ApiError::Unauthorized("admin token required".into()));
        }
        Ok(())
    }
}

/// Resolve the caller from the `Authorization` header.
pub fn resolve(state: &AppState, headers: &HeaderMap) -> Auth {
    let Some(expected) = state.admin_token.as_deref() else {
        return Auth {
            role: Role::Customer,
            configured: false,
        };
    };

    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    let role = match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => Role::Admin,
        _ => Role::Customer,
    };

    Auth {
        role,
        configured: true,
    }
}

/// Extract the token from `Authorization: Bearer <token>`.
/// The scheme is matched case-insensitively, per RFC 7235.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Compare without an early exit, so timing does not reveal how long a prefix
/// of the token matched. Length is not concealed (a length-independent compare
/// would leak it elsewhere anyway); the token should be long enough that this
/// is irrelevant.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrez"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
