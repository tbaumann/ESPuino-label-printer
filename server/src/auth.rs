//! Caller identity: a customer, or the operator.
//!
//! There is no account system. A customer's credential *is* the prefix uuid
//! they are working with (a capability). The operator proves itself in one of
//! two ways:
//!
//! - a bearer token (`ADMIN_TOKEN`) on the `Authorization` header — the CLI;
//! - a session cookie obtained by POSTing that token to `/api/admin/session` —
//!   the admin UI.
//!
//! The cookie exists so the browser never has to hold the token itself: it is
//! `HttpOnly`, so a script — including one injected by stored XSS — cannot read
//! it back out. The price of a cookie is that it is *ambient*: the browser
//! attaches it to cross-site requests too. Cookie-authenticated mutations
//! therefore also have to carry [`CSRF_HEADER`]; see [`csrf_guard`].
//!
//! Nothing here rejects an ordinary request: an absent or wrong credential
//! simply resolves to [`Role::Customer`]. Handlers decide what that means for
//! them, which keeps "customer may also do this" rules (delete a writable
//! prefix, write cards while not frozen) in one obvious place per endpoint.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use axum::extract::Request;
use axum::http::{header, HeaderMap, Method};
use axum::middleware::Next;
use axum::response::Response;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

/// Cookie holding the admin session id.
pub const SESSION_COOKIE: &str = "esplp_admin";
/// Header the admin UI must send on mutating requests. The value is arbitrary —
/// its *presence* is the point; see [`csrf_guard`].
pub const CSRF_HEADER: &str = "x-esplp-csrf";
/// Sessions expire this long after login.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Anyone who knows a prefix uuid.
    Customer,
    /// The operator.
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

/// Persisted admin sessions, keyed by session id.
///
/// Stored in the same SQLite database as the rest of the data, so a restart
/// does not log the operator out — there is no account system to log back in
/// with, so losing a session would mean re-entering the token by hand.
/// Expired rows are pruned lazily when a new session is created; a lookup
/// simply ignores them. Sessions are keyed by a random v4 uuid (122 bits), so
/// lookup by string comparison is not a practical timing oracle.
#[derive(Clone)]
pub struct Sessions {
    pool: SqlitePool,
}

impl Sessions {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Start a session, pruning expired ones while we have the connection.
    pub async fn create(&self) -> String {
        let id = Uuid::new_v4().simple().to_string();
        let expires = unix_now() + SESSION_TTL.as_secs() as i64;
        let _ = sqlx::query("DELETE FROM admin_sessions WHERE expires_at < ?1")
            .bind(unix_now())
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("INSERT INTO admin_sessions (id, expires_at) VALUES (?1, ?2)")
            .bind(&id)
            .bind(expires)
            .execute(&self.pool)
            .await;
        id
    }

    pub async fn is_valid(&self, id: &str) -> bool {
        matches!(
            sqlx::query("SELECT 1 FROM admin_sessions WHERE id = ?1 AND expires_at > ?2")
                .bind(id)
                .bind(unix_now())
                .fetch_optional(&self.pool)
                .await,
            Ok(Some(_))
        )
    }

    /// Drop one session. Returns whether it existed.
    pub async fn revoke(&self, id: &str) -> bool {
        matches!(
            sqlx::query("DELETE FROM admin_sessions WHERE id = ?1")
                .bind(id)
                .execute(&self.pool)
                .await,
            Ok(res) if res.rows_affected() > 0
        )
    }
}

/// Current unix time in whole seconds (what `admin_sessions.expires_at` holds).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Per-client throttle on failed admin-credential attempts.
///
/// Both ways an attacker can guess `ADMIN_TOKEN` — the login endpoint, and a
/// wrong bearer token on any request — feed the same counter keyed by the
/// client address. Once enough failures land inside a window, the key is
/// locked out for `lockout`; further attempts answer 429 until it lapses.
///
/// This is deliberately the coarse thing: it stops online brute force of the
/// token, not an insider with many addresses (an attacker with a botnet can
/// rotate IPs regardless of the key we pick). In-memory on purpose — a restart
/// clears the counters, which is the right failure mode (better to re-arm the
/// limiter than to lock a legitimately locked-out operator out forever).
#[derive(Clone)]
pub struct LoginLimiter {
    inner: Arc<std::sync::Mutex<HashMap<String, Bucket>>>,
    /// Failures allowed inside one `window` before a lockout begins.
    max_failures: u32,
    window: Duration,
    lockout: Duration,
}

struct Bucket {
    /// Failures recorded since `window_start` (0 once locked out).
    failures: u32,
    window_start: Instant,
    /// Until when the key refuses attempts. None while merely counting.
    blocked_until: Option<Instant>,
}

/// Defaults: 5 failures per 15 minutes buys a 15-minute lockout.
const DEFAULT_MAX_FAILURES: u32 = 5;
const DEFAULT_WINDOW: Duration = Duration::from_secs(15 * 60);
const DEFAULT_LOCKOUT: Duration = Duration::from_secs(15 * 60);

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FAILURES, DEFAULT_WINDOW, DEFAULT_LOCKOUT)
    }
}

impl LoginLimiter {
    pub fn new(max_failures: u32, window: Duration, lockout: Duration) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
            window,
            lockout,
            max_failures,
        }
    }

    /// Refuse the attempt if this key is currently locked out. Call *before*
    /// verifying, so a locked-out key never even reaches the token comparison.
    ///
    /// Prunes the key once its window and lockout have both lapsed, so the map
    /// does not grow without bound from rotating addresses.
    pub fn check(&self, key: &str) -> Result<(), ApiError> {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("limiter mutex poisoned");
        let Some(bucket) = map.get(key) else {
            return Ok(());
        };
        // Both lapsed: the attempt history is spent, drop it.
        let lock_lapsed = bucket
            .blocked_until
            .is_some_and(|b| now.duration_since(b) > self.lockout);
        let window_lapsed = now.duration_since(bucket.window_start) > self.window;
        if lock_lapsed || (bucket.blocked_until.is_none() && window_lapsed) {
            map.remove(key);
            return Ok(());
        }
        if bucket
            .blocked_until
            .is_some_and(|b| now < b)
        {
            return Err(ApiError::TooManyRequests(format!(
                "too many failed admin attempts; try again in {}s",
                bucket
                    .blocked_until
                    .map(|b| b.duration_since(now).as_secs())
                    .unwrap_or(self.lockout.as_secs())
            )));
        }
        Ok(())
    }

    /// Record one failure. Call only after a *verified-wrong* credential, so a
    /// flood of requests that do not even reach the comparison (malformed,
    /// no token configured) does not count against the key.
    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("limiter mutex poisoned");
        // A key can only have a bucket when `check` ran first, but be defensive
        // about the bucket's age in case windows were configured tiny.
        let window_lapsed = map
            .get(key)
            .is_some_and(|b| now.duration_since(b.window_start) > self.window);
        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            failures: 0,
            window_start: now,
            blocked_until: None,
        });
        if window_lapsed {
            *bucket = Bucket {
                failures: 0,
                window_start: now,
                blocked_until: None,
            };
        }
        if bucket.blocked_until.is_some() {
            return; // already locked out; a locked-out request is refused by `check` anyway
        }
        bucket.failures += 1;
        if bucket.failures >= self.max_failures {
            bucket.blocked_until = Some(now + self.lockout);
            // Keep the failing key in the map so `check` still refuses it while
            // locked; failures are zeroed so the post-lockout window starts clean.
            bucket.failures = 0;
            bucket.window_start = now;
        }
    }
}

/// The client's address, for rate-limiting.
///
/// The rate limiter keys on the network address that owns the credential
/// attempts. In front of a reverse proxy (the deployment the cookie-secure
/// logic already assumes) that is the left-most untrusted address in
/// `X-Forwarded-For`; without one it is whoever `X-Real-Ip` names. Falling back
/// to a constant when no header is present is a deliberate compromise: direct
/// clients all share one key, which at worst over-throttles (`too many failed
/// attempts` now affects every direct client at once). The endpoint cannot see
/// the peer's socket address — handlers only get the header map — so the header
/// is the only signal available. Trusting `X-Forwarded-For` is fine because the
/// limiter only ever *withholds* a credential; a spoofed header cannot mint one.
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next().map(str::trim).filter(|s| !s.is_empty()))
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.trim().is_empty())
                .map(str::trim)
        })
        .map(str::to_string)
        .unwrap_or_else(|| "<direct>".to_string())
}

/// Resolve the caller from the request headers.
///
/// The bearer token is tried first: it is the CLI's path and cannot be sent by
/// a browser on its own, so it is never the ambient credential.
///
/// A presented-but-wrong bearer token is a failed admin-credential attempt and
/// feeds the rate limiter, so an attacker cannot grind through guesses. A
/// locked-out key answers [`ApiError::TooManyRequests`] before the token is
/// even compared. Requests that carry no admin credential at all (a plain
/// customer, or a session cookie) never touch the limiter.
pub async fn resolve(state: &AppState, headers: &HeaderMap) -> Result<Auth, ApiError> {
    let configured = state.admin_token.is_some();
    let key = client_ip(headers);

    if let Some(expected) = state.admin_token.as_deref() {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_token);
        if let Some(token) = presented {
            state.login_limiter.check(&key)?;
            if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                return Ok(Auth {
                    role: Role::Admin,
                    configured,
                });
            }
            state.login_limiter.record_failure(&key);
        } else if let Some(id) = cookie_value(headers, SESSION_COOKIE) {
            if state.sessions.is_valid(&id).await {
                return Ok(Auth {
                    role: Role::Admin,
                    configured,
                });
            }
        }
    }

    Ok(Auth {
        role: Role::Customer,
        configured,
    })
}

/// Check the token submitted to the login endpoint. 503 when no token is
/// configured (the admin API is off at all), 401 on a mismatch.
pub fn verify_token(state: &AppState, candidate: &str) -> Result<(), ApiError> {
    let Some(expected) = state.admin_token.as_deref() else {
        return Err(ApiError::Unavailable(
            "admin API is disabled: ADMIN_TOKEN is not set".into(),
        ));
    };
    if constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized("invalid admin token".into()))
    }
}

/// Reject a mutating request that carries the session cookie but not the CSRF
/// header.
///
/// SameSite=Strict already stops a browser attaching the cookie to a cross-site
/// request; this is the second lock, and it is the one that still holds if the
/// cookie attributes are ever weakened. Bearer-token callers are untouched
/// (no cookie, no check) — they cannot be driven by a third-party page anyway.
pub async fn csrf_guard(req: Request, next: Next) -> Result<Response, ApiError> {
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if mutating
        && cookie_value(req.headers(), SESSION_COOKIE).is_some()
        && !has_csrf_header(req.headers())
    {
        return Err(ApiError::Forbidden(format!(
            "missing {CSRF_HEADER} header on a cookie-authenticated request"
        )));
    }
    Ok(next.run(req).await)
}

fn has_csrf_header(headers: &HeaderMap) -> bool {
    headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty())
}

/// Read one cookie out of the `Cookie` header. Values are hex, so the quoting
/// rules of RFC 6265 do not apply here.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
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
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// A fresh in-memory database with all migrations applied.
    async fn test_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations applied to the test pool");
        pool
    }

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

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

    #[test]
    fn client_ip_parses_forwarded_for() {
        let h = headers_with(&[("x-forwarded-for", "203.0.113.7, 10.0.0.2")]);
        assert_eq!(client_ip(&h), "203.0.113.7");
        // Left-most (untrusted) entry wins; whitespace is trimmed.
        let h = headers_with(&[("x-forwarded-for", " 203.0.113.9 "), ("x-real-ip", "10.1.2.3")]);
        assert_eq!(client_ip(&h), "203.0.113.9");
    }

    #[test]
    fn client_ip_falls_back() {
        // X-Real-Ip when no X-Forwarded-For.
        let h = headers_with(&[("x-real-ip", "10.1.2.3")]);
        assert_eq!(client_ip(&h), "10.1.2.3");
        // A constant when neither is present.
        let h = HeaderMap::new();
        assert_eq!(client_ip(&h), "<direct>");
    }

    /// A limiter with a tiny window and lockout so tests hit real time.
    fn tiny_limiter() -> (LoginLimiter, Duration) {
        let d = Duration::from_millis(50);
        (LoginLimiter::new(3, d, d), d)
    }

    #[test]
    fn rejects_after_max_failures() {
        let (lim, _) = tiny_limiter();
        // Two failures below the limit are allowed.
        for _ in 0..2 {
            assert!(lim.check("203.0.113.7").is_ok());
            lim.record_failure("203.0.113.7");
        }
        assert!(lim.check("203.0.113.7").is_ok());
        // Third tips it over the lockout threshold.
        lim.record_failure("203.0.113.7");
        assert!(matches!(
            lim.check("203.0.113.7"),
            Err(ApiError::TooManyRequests(_))
        ));
    }

    #[test]
    fn keys_are_independent() {
        let (lim, _) = tiny_limiter();
        for _ in 0..5 {
            lim.record_failure("attacker");
        }
        // Even after the attacker is locked out, a clean key is unaffected.
        assert!(lim.check("legit").is_ok());
    }

    #[test]
    fn lockout_lapses_and_allows_again() {
        let (lim, d) = tiny_limiter();
        for _ in 0..3 {
            lim.record_failure("203.0.113.7");
        }
        assert!(lim.check("203.0.113.7").is_err());
        std::thread::sleep(d * 3);
        assert!(lim.check("203.0.113.7").is_ok());
        // A fresh failure window starts clean after the lockout: one more miss
        // is not instantly a lockout again.
        assert!(lim.check("203.0.113.7").is_ok());
        lim.record_failure("203.0.113.7");
        assert!(lim.check("203.0.113.7").is_ok());
    }

    #[test]
    fn different_keys_share_no_counter() {
        let (lim, _) = tiny_limiter();
        lim.record_failure("a");
        lim.record_failure("b");
        assert!(lim.check("c").is_ok());
    }

    #[test]
    fn cookie_is_found_among_others() {
        let h = headers_with(&[("cookie", "a=1; esplp_admin=deadbeef; b=2")]);
        assert_eq!(
            cookie_value(&h, SESSION_COOKIE).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(cookie_value(&h, "a").as_deref(), Some("1"));
        assert_eq!(cookie_value(&h, "missing"), None);
    }

    #[test]
    fn cookie_prefix_does_not_match() {
        let h = headers_with(&[("cookie", "esplp_admin_x=nope")]);
        assert_eq!(cookie_value(&h, SESSION_COOKIE), None);
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let s = Sessions::new(test_pool().await);
        let id = s.create().await;
        assert!(s.is_valid(&id).await);
        assert!(!s.is_valid("not-a-session").await);
        assert!(s.revoke(&id).await);
        assert!(!s.is_valid(&id).await);
        // Revoking twice is not an error, just false.
        assert!(!s.revoke(&id).await);
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let s = Sessions::new(test_pool().await);
        let a = s.create().await;
        let b = s.create().await;
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn expired_session_is_rejected() {
        let s = Sessions::new(test_pool().await);
        let id = s.create().await;
        assert!(s.is_valid(&id).await);
        // Backdate the expiry into the past; only the wall clock matters now.
        sqlx::query("UPDATE admin_sessions SET expires_at = ?2 WHERE id = ?1")
            .bind(&id)
            .bind(unix_now() - 3600)
            .execute(&s.pool)
            .await
            .expect("backdate the session");
        assert!(!s.is_valid(&id).await);
    }
}
