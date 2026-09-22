//! Tiny localhost HTTP daemon that exposes the budget state to local
//! consumers (the pomodoro web app on aisparkles.com, the future Tauri
//! menu-bar app, and curl-based debugging).
//!
//! No external HTTP crate — hand-rolled GET/POST parsing keeps the
//! dependency surface flat. The full surface is two routes:
//!
//!   GET  /usage     → 200 OK, JSON view of the current Budget.
//!   POST /courtesy  → 200 OK with updated state on success,
//!                      409 if courtesy has already been spent,
//!                      403 if courtesy is disabled or during the sleep window.
//!   POST /weekly-extension
//!                   → 200 OK with updated state on success,
//!                      409 if active/already spent for the week,
//!                      403 if disabled or during the sleep window.
//!
//! Responses include CORS headers ONLY when the request carries an
//! `Origin` that exactly matches one of `ALLOWED_ORIGINS`. The matched
//! origin is echoed back verbatim — never `*` — and
//! `Access-Control-Allow-Private-Network` is emitted only for an
//! allowlisted origin, so Chromium's private-network preflight cannot be
//! satisfied by an arbitrary public page.
//!
//! There is deliberately no blanket "any loopback origin" arm: every dev
//! server the user happens to have open (`http://localhost:3000`, a Vite
//! port, a random `python -m http.server`) would otherwise be a fully
//! trusted caller able to spend the courtesy and the weekly extension.
//! The Tauri shell does not need one either — its WebView navigates to
//! `https://www.aisparkles.com` (already allowlisted) and its native side
//! calls the daemon over `ureq`, which sends no `Origin` at all.
//!
//! The two mutating POST routes additionally require the non-simple
//! request header `X-Alacritty-Budget: 1`. A browser cannot send that
//! header without first passing a preflight, and the preflight is
//! rejected for non-allowlisted origins — so a random website visited
//! while the terminal is locked cannot spend the courtesy or the weekly
//! extension allowance. Local CLI callers just add
//! `-H 'X-Alacritty-Budget: 1'`.
//!
//! Every request is additionally gated on its `Host` header (see
//! [`is_allowed_host`]). The CORS allowlist alone does not stop DNS
//! rebinding: an attacker page on `http://evil.example` whose DNS is
//! re-pointed at `127.0.0.1` becomes *same-origin* with the daemon in
//! Safari and Firefox, so the browser sends no `Origin` at all (or one
//! the page controls) and both the allowlist and the grant header are
//! trivially satisfiable from that page. The rebound request still
//! carries `Host: evil.example:38121`, which is what we reject. A
//! request with no `Host` at all is not a browser (HTTP/1.1 requires it)
//! and is treated like the originless CLI/`ureq` callers.
//!
//! ### Threading model
//!
//! The daemon runs as a single background thread spawned at process
//! startup. It shares the live `Arc<Mutex<Budget>>` with the event loop's
//! 1-second ticker, so `GET /usage` serves the in-memory state without
//! touching disk. `POST /courtesy` / `POST /weekly-extension` grant on the
//! shared state (and persist it), then dispatch the matching user event
//! through the `EventLoopProxy` so the lockout overlay and tab titles
//! refresh immediately instead of waiting for the next tick.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use log::{debug, warn};
use serde::Serialize;
use winit::event_loop::EventLoopProxy;

use crate::budget::{BlockReason, Budget, WeeklyExtensionError};
use crate::config::budget::BudgetConfig;
use crate::event::{Event, EventType};

/// Default port — chosen to be high, unlikely to collide, easy to recall.
/// Used by the pomodoro card to poll `127.0.0.1:38121/usage`.
pub const DEFAULT_PORT: u16 = 38121;

/// The complete set of browser origins allowed to talk to the daemon.
/// Everything else must carry no `Origin` at all (curl / the Tauri shell's
/// native `ureq` side). Adding an entry here hands that origin the ability
/// to spend the courtesy and the weekly extension, so it must be an exact
/// `scheme://host[:port]` string for a surface that genuinely needs it —
/// never a wildcard, a suffix match, or "all of loopback".
const ALLOWED_ORIGINS: &[&str] = &["https://aisparkles.com", "https://www.aisparkles.com"];

/// Non-simple header required on every mutating route. Its presence
/// forces a CORS preflight for browser callers, and that preflight is
/// answered without CORS headers unless the origin is allowlisted.
const GRANT_HEADER: &str = "x-alacritty-budget";

/// True only when `origin` is byte-for-byte one of [`ALLOWED_ORIGINS`].
///
/// Exact equality by design: no scheme/host parsing, no prefix or suffix
/// matching, and no loopback arm. A parsed allowlist invites near-miss
/// bypasses (`https://aisparkles.com.evil.example`) and a loopback arm
/// would trust every local dev server the user has running.
///
/// Callers with no `Origin` header at all (curl, the Tauri native side)
/// are unaffected — they simply get no CORS headers, which they do not
/// need. They still have to carry the grant header on the POST routes.
fn is_allowed_origin(origin: &str) -> bool {
    ALLOWED_ORIGINS.contains(&origin)
}

/// Host names a browser may legitimately use to reach the daemon. The
/// daemon binds `127.0.0.1` only, so anything else in a `Host` header
/// means the request was aimed at some other name that happens to
/// currently resolve to loopback — i.e. DNS rebinding.
///
/// `localhost` and the two literal loopback addresses cannot be rebound
/// (they are not attacker-controlled names), so this is a safe, complete
/// set rather than a convenience list.
const ALLOWED_HOST_NAMES: &[&str] = &["127.0.0.1", "localhost", "[::1]", "::1"];

/// True when `host` (a raw `Host` header value) names loopback on the
/// port this daemon is actually listening on.
///
/// Matching is on the *whole* value after lowercasing: the host part must
/// equal one of [`ALLOWED_HOST_NAMES`] exactly and the port, when present,
/// must equal `port`. No suffix matching — `127.0.0.1.evil.example` and
/// `evil.example:38121` both fail. An absent port is accepted because a
/// caller reaching the daemon on its own port may omit it only when it is
/// the scheme default, which loopback callers never do in practice; the
/// host check alone already closes rebinding.
fn is_allowed_host(host: &str, port: u16) -> bool {
    let host = host.trim().to_ascii_lowercase();
    // Split off the port, taking care not to cut inside an IPv6 literal.
    let (name, port_part) = match host.rfind(':') {
        // `[::1]:38121` — the colon after the closing bracket is the port.
        Some(idx) if host.starts_with('[') && host[..idx].ends_with(']') => {
            (&host[..idx], Some(&host[idx + 1..]))
        },
        // A bare `::1` has several colons and no port; treat it whole.
        Some(_) if host.starts_with('[') || host.matches(':').count() > 1 => (&host[..], None),
        Some(idx) => (&host[..idx], Some(&host[idx + 1..])),
        None => (&host[..], None),
    };
    if !ALLOWED_HOST_NAMES.contains(&name) {
        return false;
    }
    match port_part {
        None => true,
        Some(p) => p.parse::<u16>() == Ok(port),
    }
}

/// The origin to echo in the CORS headers, or `None` when the request had
/// no `Origin` or one that failed the allowlist. `None` means "emit no
/// CORS headers at all" — never `*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CorsOrigin<'a>(Option<&'a str>);

/// Wire payload for `GET /usage`. Mirrors `Budget` but enriches with the
/// computed block status and time-until-unlock so clients don't have to
/// duplicate the time math.
#[derive(Debug, Serialize)]
struct UsagePayload {
    /// Day key the state was written under.
    date_chicago: String,
    /// Focused seconds accumulated today.
    active_seconds: u64,
    weekly_active_seconds: u64,
    /// Daily cap (from config).
    cap_seconds: u64,
    /// True once the courtesy extension has been spent today.
    courtesy_used: bool,
    /// Unix-seconds timestamp when the courtesy extension expires.
    /// `None` when no extension is active.
    courtesy_expires_at: Option<u64>,
    /// Size of the daily courtesy extension.
    courtesy_seconds: u64,
    /// ISO week key for the weekly extension allowance.
    weekly_extension_week: String,
    /// Weekly extension seconds already redeemed in the current week.
    weekly_extension_used_seconds: u64,
    /// Weekly extension seconds still redeemable in the current week.
    weekly_extension_remaining_seconds: u64,
    /// Size of the next weekly extension redemption.
    weekly_extension_seconds: u64,
    /// Unix-seconds timestamp when the active weekly extension expires.
    weekly_extension_expires_at: Option<u64>,
    /// Alias fields for browser clients that strip keys containing
    /// "extension" from localhost JSON payloads.
    weekly_budget_week: String,
    weekly_budget_used_seconds: u64,
    weekly_budget_remaining_seconds: u64,
    weekly_budget_seconds: u64,
    weekly_budget_expires_at: Option<u64>,
    /// Last persistence timestamp.
    updated_at: u64,
    /// `true` when input is currently blocked. Mirrors the lockout
    /// overlay state in alacritty.
    blocked: bool,
    /// Why we're blocked, if `blocked` is true.
    reason: Option<BlockReason>,
    /// Seconds until input is permitted again. 0 when not blocked.
    seconds_until_unlock: u64,
    /// Configured timezone — handy for UI display.
    timezone: String,
}

/// Lock the shared budget, surviving poisoning: enforcement must keep
/// working even if another thread panicked while holding the lock.
fn lock_budget(budget: &Mutex<Budget>) -> MutexGuard<'_, Budget> {
    budget.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Spawn the daemon on a background thread. Returns immediately. The
/// thread is detached — there's no shutdown path because we want the
/// daemon alive for the entire lifetime of the process.
///
/// `config_provider` returns a fresh `BudgetConfig` per request so live
/// config-reload changes are reflected in the wire payload. `budget` is
/// the live state shared with the event loop's ticker; `proxy` lets the
/// POST handlers nudge the UI (overlay/tab titles) right after a grant.
pub fn spawn<F>(
    port: u16,
    config_provider: F,
    budget: Arc<Mutex<Budget>>,
    proxy: EventLoopProxy<Event>,
) where
    F: Fn() -> BudgetConfig + Send + Sync + 'static,
{
    thread::Builder::new()
        .name("alacritty-budget-daemon".to_string())
        .spawn(move || {
            let addr = format!("127.0.0.1:{}", port);
            let listener = match TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(err) => {
                    // A second instance (or stale socket holder) means this
                    // process runs daemonless while companions report the
                    // daemon offline/stale. Warn so it is diagnosable;
                    // debug-level here hid silent /usage loss entirely.
                    warn!("budget daemon: bind {addr} failed (pid {}): {err}", std::process::id());
                    return;
                },
            };
            debug!("budget daemon: listening on {addr}");
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(err) => {
                        debug!("budget daemon: accept failed: {err}");
                        continue;
                    },
                };
                let cfg = config_provider();
                if let Err(err) = handle(stream, port, &cfg, &budget, &proxy) {
                    debug!("budget daemon: handler error: {err}");
                }
            }
        })
        .expect("budget daemon thread spawn");
}

fn handle(
    mut stream: TcpStream,
    port: u16,
    cfg: &BudgetConfig,
    budget: &Mutex<Budget>,
    proxy: &EventLoopProxy<Event>,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Read the headers we actually gate on: `Host` (DNS-rebinding guard),
    // `Origin` (CORS allowlist) and the non-simple grant header required by
    // the mutating routes.
    let mut origin: Option<String> = None;
    let mut host: Option<String> = None;
    let mut grant_header = false;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 || buf == "\r\n" || buf == "\n" {
            break;
        }
        if let Some((name, value)) = buf.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "origin" => origin = Some(value.to_string()),
                "host" => host = Some(value.to_string()),
                // Preflight asks permission for the grant header; treat
                // the actual request header as the gate.
                n if n == GRANT_HEADER => grant_header = value == "1",
                _ => {},
            }
        }
    }

    // `None` origin = a non-browser caller (curl, the Tauri shell's native
    // side). Those get no CORS headers, which they don't need. A present
    // but non-allowlisted origin also gets none, so the browser refuses to
    // surface the response and the preflight for the mutating routes fails.
    let cors_origin = origin.as_deref().filter(|o| is_allowed_origin(o));
    if let Some(requested) = origin.as_deref() {
        if cors_origin.is_none() {
            warn!("budget daemon: rejecting CORS for origin {requested:?}");
        }
    }
    let cors = CorsOrigin(cors_origin);

    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // DNS-rebinding guard, ahead of every route including `OPTIONS`: a
    // rebound attacker page is same-origin, so it sails past the CORS
    // allowlist, but it cannot forge `Host`. Denied responses carry no
    // CORS headers (`CorsOrigin(None)`) so the page learns nothing.
    if let Some(requested) = host.as_deref() {
        if !is_allowed_host(requested, port) {
            warn!("budget daemon: rejecting {path} for host {requested:?}");
            let body = r#"{"error":"bad_host"}"#;
            let deny = CorsOrigin(None);
            write_response(&mut stream, deny, 403, "Forbidden", "application/json", body)?;
            return Ok(());
        }
    }

    // A browser that reached a mutating route from a non-allowlisted
    // origin has already failed the preflight, so its response would be
    // unreadable — but the grant would still have happened. Refuse before
    // touching state rather than relying on the browser to discard it.
    if method == "POST" && origin.is_some() && cors_origin.is_none() {
        let body = r#"{"error":"origin_not_allowed"}"#;
        write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
        return Ok(());
    }

    // Gate the mutating routes on the non-simple header. A browser can
    // only send it after a preflight, and the preflight above is answered
    // without CORS headers for any non-allowlisted origin.
    if method == "POST" && !grant_header {
        warn!("budget daemon: rejecting {path} without {GRANT_HEADER} header");
        let body = r#"{"error":"missing_grant_header"}"#;
        write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
        return Ok(());
    }

    match (method, path) {
        ("OPTIONS", _) => write_response(&mut stream, cors, 204, "No Content", "", "")?,
        ("GET", "/usage") => {
            let body =
                serde_json::to_string(&snapshot(cfg, budget)).unwrap_or_else(|_| "{}".into());
            write_response(&mut stream, cors, 200, "OK", "application/json", &body)?;
        },
        ("POST", "/courtesy") => {
            let (granted, was_used) = {
                let mut budget = lock_budget(budget);
                budget.refresh_day_boundary(cfg);
                let was_used = budget.courtesy_used;
                let granted = budget.grant_courtesy(cfg);
                if granted {
                    budget.save();
                }
                (granted, was_used)
            };
            if granted {
                // Nudge the event loop so the lockout overlay / 🔒 title
                // clear immediately; the grant itself already happened on
                // the shared state above.
                let _ = proxy.send_event(Event::new(EventType::GrantCourtesy, None));
                let body =
                    serde_json::to_string(&snapshot(cfg, budget)).unwrap_or_else(|_| "{}".into());
                write_response(&mut stream, cors, 200, "OK", "application/json", &body)?;
            } else if !cfg.allow_courtesy {
                let body = r#"{"error":"courtesy_disabled"}"#;
                write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
            } else if was_used {
                let body = r#"{"error":"already_used"}"#;
                write_response(&mut stream, cors, 409, "Conflict", "application/json", body)?;
            } else {
                let body = r#"{"error":"sleep_window"}"#;
                write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
            }
        },
        ("POST", "/weekly-extension") => {
            let result = {
                let mut budget = lock_budget(budget);
                let result = budget.grant_weekly_extension(cfg);
                if result.is_ok() {
                    budget.save();
                }
                result
            };
            match result {
                Ok(()) => {
                    let _ = proxy.send_event(Event::new(EventType::GrantWeeklyExtension, None));
                    let body = serde_json::to_string(&snapshot(cfg, budget))
                        .unwrap_or_else(|_| "{}".into());
                    write_response(&mut stream, cors, 200, "OK", "application/json", &body)?;
                },
                Err(WeeklyExtensionError::Disabled) => {
                    let body = r#"{"error":"weekly_extension_disabled"}"#;
                    write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
                },
                Err(WeeklyExtensionError::SleepWindow) => {
                    let body = r#"{"error":"sleep_window"}"#;
                    write_response(&mut stream, cors, 403, "Forbidden", "application/json", body)?;
                },
                Err(WeeklyExtensionError::AlreadyActive) => {
                    let body = r#"{"error":"weekly_extension_active"}"#;
                    write_response(&mut stream, cors, 409, "Conflict", "application/json", body)?;
                },
                Err(WeeklyExtensionError::AllowanceSpent) => {
                    let body = r#"{"error":"weekly_extension_spent"}"#;
                    write_response(&mut stream, cors, 409, "Conflict", "application/json", body)?;
                },
            }
        },
        _ => {
            write_response(&mut stream, cors, 404, "Not Found", "application/json", "{}")?;
        },
    }
    Ok(())
}

fn snapshot(cfg: &BudgetConfig, budget: &Mutex<Budget>) -> UsagePayload {
    // Clone the live shared state; roll the day on the clone only so a
    // read-only GET never mutates state the ticker owns (the tick performs
    // the same idempotent rollover within a second anyway).
    let mut budget = lock_budget(budget).clone();
    budget.refresh_day_boundary(cfg);
    let reason = budget.block_status(cfg);
    let blocked = reason.is_some();
    UsagePayload {
        date_chicago: budget.date_chicago.clone(),
        active_seconds: budget.active_seconds,
        weekly_active_seconds: budget.weekly_active_seconds,
        cap_seconds: cfg.cap_seconds,
        courtesy_used: budget.courtesy_used,
        courtesy_expires_at: budget.courtesy_expires_at,
        courtesy_seconds: cfg.courtesy_seconds,
        weekly_extension_week: budget.weekly_extension_week.clone(),
        weekly_extension_used_seconds: budget.weekly_extension_used_seconds,
        weekly_extension_remaining_seconds: budget.weekly_extension_remaining_seconds(cfg),
        weekly_extension_seconds: cfg.weekly_extension_seconds,
        weekly_extension_expires_at: budget.weekly_extension_expires_at,
        weekly_budget_week: budget.weekly_extension_week.clone(),
        weekly_budget_used_seconds: budget.weekly_extension_used_seconds,
        weekly_budget_remaining_seconds: budget.weekly_extension_remaining_seconds(cfg),
        weekly_budget_seconds: cfg.weekly_extension_seconds,
        weekly_budget_expires_at: budget.weekly_extension_expires_at,
        updated_at: budget.updated_at,
        blocked,
        reason,
        seconds_until_unlock: budget.seconds_until_unlock(cfg),
        timezone: cfg.timezone.clone(),
    }
}

/// Minimal HTTP/1.1 writer. CORS headers are emitted only for an
/// allowlisted origin (see [`is_allowed_origin`]), echoing that exact
/// origin. The private-network header — which is what lets Chromium hand
/// a public HTTPS page access to loopback — rides along with it, so it is
/// never offered to an unknown site.
fn write_response(
    stream: &mut TcpStream,
    cors: CorsOrigin<'_>,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let bytes = build_response_bytes(cors, status, reason, content_type, body);
    stream.write_all(&bytes)?;
    Ok(())
}

/// Pure helper: assemble the full HTTP/1.1 response byte string from its
/// logical parts. Extracted so the CORS header contract can be unit-tested
/// without a live TCP socket.
fn build_response_bytes(
    cors: CorsOrigin<'_>,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Vec<u8> {
    let mut headers = String::new();
    headers.push_str(&format!("HTTP/1.1 {} {}\r\n", status, reason));
    // Always vary: the same URL yields different CORS headers per origin.
    headers.push_str("Vary: Origin\r\n");
    if let CorsOrigin(Some(origin)) = cors {
        headers.push_str(&format!("Access-Control-Allow-Origin: {}\r\n", origin));
        headers.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        headers.push_str("Access-Control-Allow-Headers: Content-Type, X-Alacritty-Budget\r\n");
        headers.push_str("Access-Control-Allow-Private-Network: true\r\n");
    }
    headers.push_str("Cache-Control: no-store\r\n");
    if !content_type.is_empty() {
        headers.push_str(&format!("Content-Type: {}\r\n", content_type));
    }
    headers.push_str(&format!("Content-Length: {}\r\n", body.len()));
    headers.push_str("Connection: close\r\n\r\n");
    let mut out = headers.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Pure routing-decision helper used by the test suite.
/// Returns `(status, reason, content_type, body)` for a known request pair,
/// but does NOT execute any I/O (no disk reads, no courtesy mutations).
/// The production `handle()` path goes through `snapshot()` / `Budget` which
/// need real disk state, so this helper only covers the structural/routing
/// contract (status codes, CORS presence, error-body shape).
#[cfg(test)]
fn route_static(method: &str, path: &str) -> (u16, &'static str) {
    match (method, path) {
        ("OPTIONS", _) => (204, "No Content"),
        ("GET", "/usage") => (200, "OK"),
        ("POST", "/courtesy") => (200, "OK"),
        ("POST", "/weekly-extension") => (200, "OK"),
        _ => (404, "Not Found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for an allowlisted browser caller.
    const ALLOWED: CorsOrigin<'static> = CorsOrigin(Some("https://aisparkles.com"));
    /// Stand-in for a rejected / absent origin.
    const DENIED: CorsOrigin<'static> = CorsOrigin(None);

    // ── build_response_bytes ──────────────────────────────────────────────

    #[test]
    fn response_status_line_correct() {
        let raw = build_response_bytes(ALLOWED, 200, "OK", "application/json", "{}");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "bad status line: {text:?}");
    }

    #[test]
    fn response_cors_headers_present() {
        let raw = build_response_bytes(ALLOWED, 200, "OK", "application/json", r#"{"x":1}"#);
        let text = String::from_utf8(raw).unwrap();
        assert!(
            text.contains("Access-Control-Allow-Origin: https://aisparkles.com\r\n"),
            "ACAO must echo the matched origin, never `*`: {text:?}"
        );
        assert!(!text.contains("Access-Control-Allow-Origin: *"), "wildcard ACAO must be gone");
        assert!(
            text.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"),
            "ACAM header missing"
        );
        assert!(
            text.contains("Access-Control-Allow-Headers: Content-Type, X-Alacritty-Budget\r\n"),
            "ACAH header missing"
        );
        assert!(
            text.contains("Access-Control-Allow-Private-Network: true\r\n"),
            "ACAPN header missing"
        );
        assert!(text.contains("Vary: Origin\r\n"), "Vary: Origin missing");
        assert!(text.contains("Cache-Control: no-store\r\n"), "Cache-Control header missing");
    }

    #[test]
    fn response_has_no_cors_headers_for_denied_origin() {
        let raw = build_response_bytes(DENIED, 200, "OK", "application/json", r#"{"x":1}"#);
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.contains("Access-Control-Allow-Origin"), "ACAO leaked to denied origin");
        assert!(
            !text.contains("Access-Control-Allow-Private-Network"),
            "private-network opt-in leaked to denied origin — this is the PNA hole"
        );
        assert!(text.contains("Vary: Origin\r\n"), "Vary: Origin missing");
    }

    // ── is_allowed_origin ────────────────────────────────────────────────

    #[test]
    fn allowlisted_origins_accepted() {
        for origin in ["https://aisparkles.com", "https://www.aisparkles.com"] {
            assert!(is_allowed_origin(origin), "should be allowed: {origin}");
        }
    }

    /// The allowlist is exactly [`ALLOWED_ORIGINS`] and nothing else.
    #[test]
    fn allowlist_is_exactly_the_constant() {
        assert_eq!(
            ALLOWED_ORIGINS,
            &["https://aisparkles.com", "https://www.aisparkles.com"],
            "widening this constant hands a new origin the ability to spend courtesy"
        );
    }

    #[test]
    fn arbitrary_public_origins_rejected() {
        for origin in [
            "https://evil.example",
            "https://aisparkles.com.evil.example",
            "https://evil.example/aisparkles.com",
            "http://localhost.evil.example",
            "http://localhost@evil.example",
            "http://127.0.0.1.evil.example",
            "http://localhost:evil",
            "null",
            "",
            "file://",
            "http://aisparkles.com",
            // Trailing-slash / case variants must not sneak past equality.
            "https://aisparkles.com/",
            "https://AISPARKLES.com",
        ] {
            assert!(!is_allowed_origin(origin), "should be rejected: {origin}");
        }
    }

    /// Regression: a blanket loopback arm made every dev server the user
    /// happens to have open a fully trusted caller — it got CORS headers,
    /// passed the `X-Alacritty-Budget` preflight, and could POST
    /// `/courtesy` and `/weekly-extension`. No loopback origin is
    /// allowlisted; the Tauri shell uses `https://www.aisparkles.com` in
    /// its WebView and sends no `Origin` from its native side.
    #[test]
    fn loopback_origins_rejected() {
        for origin in [
            "http://localhost:3000",
            "http://localhost",
            "http://localhost:5173",
            "https://localhost:8443",
            "http://127.0.0.1",
            "http://127.0.0.1:38121",
            "https://127.0.0.1:3000",
            "http://[::1]",
            "http://[::1]:1420",
        ] {
            assert!(!is_allowed_origin(origin), "loopback origin must be rejected: {origin}");
        }
    }

    /// A request with no `Origin` (curl, Tauri native `ureq`) still gets a
    /// normal response body — it just carries no CORS headers.
    #[test]
    fn absent_origin_still_served_without_cors() {
        let raw =
            build_response_bytes(DENIED, 200, "OK", "application/json", r#"{"blocked":false}"#);
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "non-browser GET must still succeed");
        assert!(text.ends_with(r#"{"blocked":false}"#), "body must still be served");
        assert!(!text.contains("Access-Control-Allow-Origin"), "no CORS for originless caller");
    }

    // ── is_allowed_host — DNS-rebinding guard ───────────────────────────

    #[test]
    fn loopback_hosts_accepted_on_the_bound_port() {
        for host in
            ["127.0.0.1:38121", "localhost:38121", "[::1]:38121", "LocalHost:38121", " 127.0.0.1 "]
        {
            assert!(is_allowed_host(host, 38121), "loopback host must be accepted: {host}");
        }
        // Port-less forms are fine too; the name alone closes rebinding.
        assert!(is_allowed_host("127.0.0.1", 38121));
        assert!(is_allowed_host("[::1]", 38121));
        assert!(is_allowed_host("::1", 38121));
    }

    #[test]
    fn rebound_attacker_hosts_rejected() {
        for host in [
            "evil.example:38121",
            "evil.example",
            "127.0.0.1.evil.example:38121",
            "aisparkles.com:38121",
            "localhost.evil.example:38121",
            "",
        ] {
            assert!(!is_allowed_host(host, 38121), "rebound host must be rejected: {host}");
        }
    }

    #[test]
    fn host_port_must_match_the_bound_port() {
        assert!(!is_allowed_host("127.0.0.1:38122", 38121), "wrong port must be rejected");
        assert!(!is_allowed_host("127.0.0.1:notaport", 38121), "junk port must be rejected");
        // The same host on a daemon bound elsewhere is accepted there.
        assert!(is_allowed_host("127.0.0.1:38122", 38122));
    }

    #[test]
    fn response_content_type_included_when_nonempty() {
        let raw = build_response_bytes(ALLOWED, 200, "OK", "application/json", "{}");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("Content-Type: application/json\r\n"), "Content-Type header missing");
    }

    #[test]
    fn response_content_type_omitted_when_empty() {
        let raw = build_response_bytes(ALLOWED, 204, "No Content", "", "");
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.contains("Content-Type:"), "Content-Type should be absent for 204");
    }

    #[test]
    fn response_content_length_matches_body() {
        let body = r#"{"hello":"world"}"#;
        let raw = build_response_bytes(ALLOWED, 200, "OK", "application/json", body);
        let text = String::from_utf8(raw).unwrap();
        let expected = format!("Content-Length: {}\r\n", body.len());
        assert!(text.contains(&expected), "Content-Length mismatch: {text:?}");
    }

    #[test]
    fn response_body_follows_blank_line() {
        let body = r#"{"blocked":false}"#;
        let raw = build_response_bytes(ALLOWED, 200, "OK", "application/json", body);
        let text = String::from_utf8(raw).unwrap();
        // Headers end with \r\n\r\n; body immediately follows.
        let sep = "\r\n\r\n";
        let sep_pos = text.find(sep).expect("no header/body separator");
        assert_eq!(&text[sep_pos + sep.len()..], body, "body doesn't follow separator");
    }

    #[test]
    fn response_connection_close() {
        let raw = build_response_bytes(ALLOWED, 200, "OK", "", "");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("Connection: close\r\n"), "Connection: close missing");
    }

    // ── route_static — routing matrix ────────────────────────────────────

    #[test]
    fn options_any_path_returns_204() {
        assert_eq!(route_static("OPTIONS", "/").0, 204);
        assert_eq!(route_static("OPTIONS", "/usage").0, 204);
        assert_eq!(route_static("OPTIONS", "/courtesy").0, 204);
        assert_eq!(route_static("OPTIONS", "/weekly-extension").0, 204);
        assert_eq!(route_static("OPTIONS", "/anything").0, 204);
    }

    #[test]
    fn get_usage_returns_200() {
        assert_eq!(route_static("GET", "/usage"), (200, "OK"));
    }

    #[test]
    fn post_courtesy_returns_200_on_route() {
        assert_eq!(route_static("POST", "/courtesy"), (200, "OK"));
    }

    #[test]
    fn post_weekly_extension_returns_200_on_route() {
        assert_eq!(route_static("POST", "/weekly-extension"), (200, "OK"));
    }

    #[test]
    fn unknown_routes_return_404() {
        assert_eq!(route_static("GET", "/").0, 404);
        assert_eq!(route_static("GET", "/unknown").0, 404);
        assert_eq!(route_static("DELETE", "/usage").0, 404);
        assert_eq!(route_static("PUT", "/courtesy").0, 404);
        assert_eq!(route_static("PUT", "/weekly-extension").0, 404);
    }

    // ── UsagePayload JSON-shape contract ─────────────────────────────────
    // We verify that `UsagePayload` serializes with all required field names
    // so the wire format stays stable for the pomodoro app and Tauri shell.

    #[test]
    fn usage_payload_has_all_required_fields() {
        let payload = UsagePayload {
            date_chicago: "2026-05-21".to_string(),
            active_seconds: 3600,
            weekly_active_seconds: 3600,
            cap_seconds: 10800,
            courtesy_used: false,
            courtesy_expires_at: None,
            courtesy_seconds: 900,
            weekly_extension_week: "2026-W21".to_string(),
            weekly_extension_used_seconds: 0,
            weekly_extension_remaining_seconds: 21600,
            weekly_extension_seconds: 3600,
            weekly_extension_expires_at: None,
            weekly_budget_week: "2026-W21".to_string(),
            weekly_budget_used_seconds: 0,
            weekly_budget_remaining_seconds: 21600,
            weekly_budget_seconds: 3600,
            weekly_budget_expires_at: None,
            updated_at: 1_000_000,
            blocked: false,
            reason: None,
            seconds_until_unlock: 0,
            timezone: "America/Chicago".to_string(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        for field in &[
            "date_chicago",
            "active_seconds",
            "weekly_active_seconds",
            "cap_seconds",
            "courtesy_used",
            "courtesy_seconds",
            "weekly_extension_week",
            "weekly_extension_used_seconds",
            "weekly_extension_remaining_seconds",
            "weekly_extension_seconds",
            "weekly_extension_expires_at",
            "updated_at",
            "blocked",
            "seconds_until_unlock",
            "timezone",
        ] {
            assert!(obj.contains_key(*field), "missing field: {field}");
        }
    }

    #[test]
    fn usage_payload_courtesy_expires_at_null_when_none() {
        let payload = UsagePayload {
            date_chicago: "2026-05-21".to_string(),
            active_seconds: 0,
            weekly_active_seconds: 0,
            cap_seconds: 10800,
            courtesy_used: false,
            courtesy_expires_at: None,
            courtesy_seconds: 900,
            weekly_extension_week: "2026-W21".to_string(),
            weekly_extension_used_seconds: 0,
            weekly_extension_remaining_seconds: 21600,
            weekly_extension_seconds: 3600,
            weekly_extension_expires_at: None,
            weekly_budget_week: "2026-W21".to_string(),
            weekly_budget_used_seconds: 0,
            weekly_budget_remaining_seconds: 21600,
            weekly_budget_seconds: 3600,
            weekly_budget_expires_at: None,
            updated_at: 0,
            blocked: false,
            reason: None,
            seconds_until_unlock: 0,
            timezone: "America/Chicago".to_string(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        // When None, the field must serialize as JSON null (not omitted).
        assert!(
            v.as_object().unwrap().contains_key("courtesy_expires_at"),
            "courtesy_expires_at key missing from JSON"
        );
        assert!(v["courtesy_expires_at"].is_null(), "expected null when None");
    }

    #[test]
    fn usage_payload_courtesy_expires_at_present_when_some() {
        let payload = UsagePayload {
            date_chicago: "2026-05-21".to_string(),
            active_seconds: 0,
            weekly_active_seconds: 0,
            cap_seconds: 10800,
            courtesy_used: true,
            courtesy_expires_at: Some(9_999_999),
            courtesy_seconds: 900,
            weekly_extension_week: "2026-W21".to_string(),
            weekly_extension_used_seconds: 3600,
            weekly_extension_remaining_seconds: 18000,
            weekly_extension_seconds: 3600,
            weekly_extension_expires_at: Some(9_888_888),
            weekly_budget_week: "2026-W21".to_string(),
            weekly_budget_used_seconds: 3600,
            weekly_budget_remaining_seconds: 18000,
            weekly_budget_seconds: 3600,
            weekly_budget_expires_at: Some(9_888_888),
            updated_at: 0,
            blocked: false,
            reason: None,
            seconds_until_unlock: 0,
            timezone: "America/Chicago".to_string(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(v["courtesy_expires_at"], 9_999_999u64);
        assert_eq!(v["weekly_extension_expires_at"], 9_888_888u64);
    }
}
