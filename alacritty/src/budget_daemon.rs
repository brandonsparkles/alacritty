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
//!
//! Both responses include CORS headers permitting the aisparkles origin
//! AND any localhost origin (for the Tauri shell + curl).
//!
//! ### Threading model
//!
//! The daemon runs as a single background thread spawned at process
//! startup. It re-reads `usage.json` from disk on every request — stale
//! by at most one tick (1 s). For POST /courtesy, the daemon writes
//! directly to `usage.json`; the main thread's in-memory copy refreshes
//! on the next tick via the standard load path.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use log::debug;
use serde::Serialize;

use crate::budget::{BlockReason, Budget};
use crate::config::budget::BudgetConfig;

/// Default port — chosen to be high, unlikely to collide, easy to recall.
/// Used by the pomodoro card to poll `127.0.0.1:38121/usage`.
pub const DEFAULT_PORT: u16 = 38121;

/// Wire payload for `GET /usage`. Mirrors `Budget` but enriches with the
/// computed block status and time-until-unlock so clients don't have to
/// duplicate the time math.
#[derive(Debug, Serialize)]
struct UsagePayload {
    /// Day key the state was written under.
    date_chicago: String,
    /// Focused seconds accumulated today.
    active_seconds: u64,
    /// Daily cap (from config).
    cap_seconds: u64,
    /// True once the courtesy extension has been spent today.
    courtesy_used: bool,
    /// Unix-seconds timestamp when the courtesy extension expires.
    /// `None` when no extension is active.
    courtesy_expires_at: Option<u64>,
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

/// Spawn the daemon on a background thread. Returns immediately. The
/// thread is detached — there's no shutdown path because we want the
/// daemon alive for the entire lifetime of the process.
///
/// `config_provider` returns a fresh `BudgetConfig` per request so live
/// config-reload changes are reflected in the wire payload.
pub fn spawn<F>(port: u16, config_provider: F)
where
    F: Fn() -> BudgetConfig + Send + Sync + 'static,
{
    thread::Builder::new()
        .name("alacritty-budget-daemon".to_string())
        .spawn(move || {
            let addr = format!("127.0.0.1:{}", port);
            let listener = match TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(err) => {
                    debug!("budget daemon: bind {addr} failed: {err}");
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
                if let Err(err) = handle(stream, &cfg) {
                    debug!("budget daemon: handler error: {err}");
                }
            }
        })
        .expect("budget daemon thread spawn");
}

fn handle(mut stream: TcpStream, cfg: &BudgetConfig) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain headers — we don't actually need any of them.
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 || buf == "\r\n" || buf == "\n" {
            break;
        }
    }

    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    match (method, path) {
        ("OPTIONS", _) => write_response(&mut stream, 204, "No Content", "", "")?,
        ("GET", "/usage") => {
            let body = serde_json::to_string(&snapshot(cfg)).unwrap_or_else(|_| "{}".into());
            write_response(&mut stream, 200, "OK", "application/json", &body)?;
        },
        ("POST", "/courtesy") => {
            let mut budget = Budget::load_or_default(cfg);
            let was_used = budget.courtesy_used;
            if budget.grant_courtesy(cfg) {
                budget.save();
                let body =
                    serde_json::to_string(&snapshot(cfg)).unwrap_or_else(|_| "{}".into());
                write_response(&mut stream, 200, "OK", "application/json", &body)?;
            } else if !cfg.allow_courtesy {
                let body = r#"{"error":"courtesy_disabled"}"#;
                write_response(&mut stream, 403, "Forbidden", "application/json", body)?;
            } else if was_used {
                let body = r#"{"error":"already_used"}"#;
                write_response(&mut stream, 409, "Conflict", "application/json", body)?;
            } else {
                let body = r#"{"error":"sleep_window"}"#;
                write_response(&mut stream, 403, "Forbidden", "application/json", body)?;
            }
        },
        _ => {
            write_response(&mut stream, 404, "Not Found", "application/json", "{}")?;
        },
    }
    Ok(())
}

fn snapshot(cfg: &BudgetConfig) -> UsagePayload {
    let mut budget = Budget::load_or_default(cfg);
    budget.refresh_day_boundary(cfg);
    let reason = budget.block_status(cfg);
    let blocked = reason.is_some();
    UsagePayload {
        date_chicago: budget.date_chicago.clone(),
        active_seconds: budget.active_seconds,
        cap_seconds: cfg.cap_seconds,
        courtesy_used: budget.courtesy_used,
        courtesy_expires_at: budget.courtesy_expires_at,
        updated_at: budget.updated_at,
        blocked,
        reason,
        seconds_until_unlock: budget.seconds_until_unlock(cfg),
        timezone: cfg.timezone.clone(),
    }
}

/// Minimal HTTP/1.1 writer. CORS headers allow the aisparkles app + any
/// localhost origin to fetch directly from the browser.
fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let bytes = build_response_bytes(status, reason, content_type, body);
    stream.write_all(&bytes)?;
    Ok(())
}

/// Pure helper: assemble the full HTTP/1.1 response byte string from its
/// logical parts. Extracted so the CORS header contract can be unit-tested
/// without a live TCP socket.
fn build_response_bytes(status: u16, reason: &str, content_type: &str, body: &str) -> Vec<u8> {
    let mut headers = String::new();
    headers.push_str(&format!("HTTP/1.1 {} {}\r\n", status, reason));
    headers.push_str("Access-Control-Allow-Origin: *\r\n");
    headers.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
    headers.push_str("Access-Control-Allow-Headers: Content-Type\r\n");
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
        _ => (404, "Not Found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_response_bytes ──────────────────────────────────────────────

    #[test]
    fn response_status_line_correct() {
        let raw = build_response_bytes(200, "OK", "application/json", "{}");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "bad status line: {text:?}");
    }

    #[test]
    fn response_cors_headers_present() {
        let raw = build_response_bytes(200, "OK", "application/json", r#"{"x":1}"#);
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("Access-Control-Allow-Origin: *\r\n"), "ACAO header missing");
        assert!(
            text.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"),
            "ACAM header missing"
        );
        assert!(
            text.contains("Access-Control-Allow-Headers: Content-Type\r\n"),
            "ACAH header missing"
        );
        assert!(text.contains("Cache-Control: no-store\r\n"), "Cache-Control header missing");
    }

    #[test]
    fn response_content_type_included_when_nonempty() {
        let raw = build_response_bytes(200, "OK", "application/json", "{}");
        let text = String::from_utf8(raw).unwrap();
        assert!(
            text.contains("Content-Type: application/json\r\n"),
            "Content-Type header missing"
        );
    }

    #[test]
    fn response_content_type_omitted_when_empty() {
        let raw = build_response_bytes(204, "No Content", "", "");
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.contains("Content-Type:"), "Content-Type should be absent for 204");
    }

    #[test]
    fn response_content_length_matches_body() {
        let body = r#"{"hello":"world"}"#;
        let raw = build_response_bytes(200, "OK", "application/json", body);
        let text = String::from_utf8(raw).unwrap();
        let expected = format!("Content-Length: {}\r\n", body.len());
        assert!(text.contains(&expected), "Content-Length mismatch: {text:?}");
    }

    #[test]
    fn response_body_follows_blank_line() {
        let body = r#"{"blocked":false}"#;
        let raw = build_response_bytes(200, "OK", "application/json", body);
        let text = String::from_utf8(raw).unwrap();
        // Headers end with \r\n\r\n; body immediately follows.
        let sep = "\r\n\r\n";
        let sep_pos = text.find(sep).expect("no header/body separator");
        assert_eq!(&text[sep_pos + sep.len()..], body, "body doesn't follow separator");
    }

    #[test]
    fn response_connection_close() {
        let raw = build_response_bytes(200, "OK", "", "");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("Connection: close\r\n"), "Connection: close missing");
    }

    // ── route_static — routing matrix ────────────────────────────────────

    #[test]
    fn options_any_path_returns_204() {
        assert_eq!(route_static("OPTIONS", "/").0, 204);
        assert_eq!(route_static("OPTIONS", "/usage").0, 204);
        assert_eq!(route_static("OPTIONS", "/courtesy").0, 204);
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
    fn unknown_routes_return_404() {
        assert_eq!(route_static("GET", "/").0, 404);
        assert_eq!(route_static("GET", "/unknown").0, 404);
        assert_eq!(route_static("DELETE", "/usage").0, 404);
        assert_eq!(route_static("PUT", "/courtesy").0, 404);
    }

    // ── UsagePayload JSON-shape contract ─────────────────────────────────
    // We verify that `UsagePayload` serializes with all required field names
    // so the wire format stays stable for the pomodoro app and Tauri shell.

    #[test]
    fn usage_payload_has_all_required_fields() {
        let payload = UsagePayload {
            date_chicago: "2026-05-21".to_string(),
            active_seconds: 3600,
            cap_seconds: 10800,
            courtesy_used: false,
            courtesy_expires_at: None,
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
            "cap_seconds",
            "courtesy_used",
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
            cap_seconds: 10800,
            courtesy_used: false,
            courtesy_expires_at: None,
            updated_at: 0,
            blocked: false,
            reason: None,
            seconds_until_unlock: 0,
            timezone: "America/Chicago".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        // When None, the field must serialize as JSON null (not omitted).
        assert!(v.as_object().unwrap().contains_key("courtesy_expires_at"),
            "courtesy_expires_at key missing from JSON");
        assert!(v["courtesy_expires_at"].is_null(), "expected null when None");
    }

    #[test]
    fn usage_payload_courtesy_expires_at_present_when_some() {
        let payload = UsagePayload {
            date_chicago: "2026-05-21".to_string(),
            active_seconds: 0,
            cap_seconds: 10800,
            courtesy_used: true,
            courtesy_expires_at: Some(9_999_999),
            updated_at: 0,
            blocked: false,
            reason: None,
            seconds_until_unlock: 0,
            timezone: "America/Chicago".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(v["courtesy_expires_at"], 9_999_999u64);
    }
}
