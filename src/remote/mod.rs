// Remote log console — self-contained core.
//
// A hand-rolled HTTP/1.1 server (std::net, thread-per-connection, no async,
// no new crate) plus routing for the read-only log console + remote pause
// trigger. This module is the SESSION-1 deliverable: it compiles and is unit-
// tested on Linux but is NOT yet spawned by the service (that wiring, plus the
// Win32 hostname/firewall bits, is session 2).
//
// Design: TCP I/O lives in the thin `server`/`handle_connection`/`stream_logs`
// functions; every routing decision is in the pure `route` function, which
// takes injected dependencies (`Ctx`) plus the current `Instant`/`NaiveDate`
// and returns a value — so the whole surface is testable without a socket.
//
// On Windows the whole surface is live: the service spawns `server`, which
// chains through `route` into every handler. Off Windows there is no such
// caller (the service + config UI are `#[cfg(windows)]`) — only the unit tests
// exercise the core — so a non-test Linux build sees the HTTP-serving surface
// as unused. Suppress dead_code there only, keeping full enforcement on the
// shipping Windows build.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod assets;
pub mod http;
pub mod logs;
pub mod session;

use std::io::{self, BufReader, Cursor, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::modes::config::lockout::{Lockout, LockoutState};

use assets::INDEX_HTML;
use http::{parse_request, sse_data, write_sse_headers, Request, Response, SSE_PING};
use logs::{history_files, tail_snapshot, today_path, LogDirs, LogFile, Tailer};
use session::Sessions;

/// Fixed LAN port for the console. Not configurable in v1.
pub const REMOTE_LOG_PORT: u16 = 8787;

/// `<host>.local:<port>` — the value `/api/session` returns as `hostname` (the
/// page prepends `http://`), and the value the service passes to `server`. The
/// config UI wraps it as `http://<…>/`. Single source so both call sites agree.
pub fn lan_host_port() -> String {
    format!("{}.local:{}", computer_name(), REMOTE_LOG_PORT)
}

/// Bare DNS host name of this machine. Uses `GetComputerNameExW` on Windows,
/// falling back to `%COMPUTERNAME%` (also the sole source off-Windows, so the
/// crate still builds + tests on Linux).
#[cfg(windows)]
fn computer_name() -> String {
    use windows::core::PWSTR;
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};

    // First call sizes the buffer (returns ERROR_MORE_DATA, sets `size` to the
    // length incl. the NUL); the second fills it and sets `size` to the length
    // excl. the NUL.
    let mut size: u32 = 0;
    unsafe {
        let _ = GetComputerNameExW(ComputerNameDnsHostname, None, &mut size);
    }
    if size > 0 {
        let mut buf = vec![0u16; size as usize];
        let ok = unsafe {
            GetComputerNameExW(
                ComputerNameDnsHostname,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut size,
            )
        };
        if ok.is_ok() {
            return String::from_utf16_lossy(&buf[..size as usize]);
        }
    }
    env_computer_name()
}

#[cfg(not(windows))]
fn computer_name() -> String {
    env_computer_name()
}

fn env_computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_string())
}

/// SSE tail poll cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Emit a `: ping` keep-alive after this many idle polls (~15 s).
const PING_EVERY_POLLS: u32 = 15;

/// Injected dependencies shared by every connection. Keeps `TickCommand` and
/// the tick loop out of this module: the service hands in a closure and the
/// log directories, so nothing here depends on the Windows service internals.
pub struct Ctx {
    /// Path to `state.dat`; read fresh on `/api/auth` (login is rare, so the
    /// DPAPI decrypt cost is fine and avoids sharing the tick loop's config).
    cfg_path: PathBuf,
    dirs: LogDirs,
    sessions: Sessions,
    lockout: Mutex<Lockout>,
    /// `http://<hostname>.local:<port>/` display value. Empty in the core;
    /// the service fills it from `GetComputerNameExW` in session 2.
    hostname: String,
    /// Resets the next interval pause (preserving the noon pause) — the
    /// service builds this from `mpsc::Sender<TickCommand>`.
    trigger_pause: Box<dyn Fn() + Send + Sync>,
}

impl Ctx {
    pub fn new(
        cfg_path: PathBuf,
        dirs: LogDirs,
        hostname: String,
        trigger_pause: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Ctx {
            cfg_path,
            dirs,
            sessions: Sessions::new(),
            lockout: Mutex::new(Lockout::new()),
            hostname,
            trigger_pause,
        }
    }
}

/// What the router decided to do with a request: send a buffered response, or
/// hand off to the SSE streamer for a log file.
pub enum RouteOutcome {
    Resp(Response),
    Stream(LogFile),
}

/// Pure request router. `now` drives the session/lockout clocks; `today`
/// selects the dated log file.
pub fn route(req: &Request, ctx: &Ctx, now: Instant, today: NaiveDate) -> RouteOutcome {
    use RouteOutcome::{Resp, Stream};

    let authed = req
        .cookie("session")
        .is_some_and(|t| ctx.sessions.validate(t, now));

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Resp(Response::html(INDEX_HTML)),

        ("GET", "/api/session") => {
            let body = serde_json::json!({
                "authed": authed,
                "hostname": ctx.hostname,
                "logs": LogFile::ALL.map(|f| f.param()),
            })
            .to_string();
            Resp(Response::ok_json(body))
        }

        ("POST", "/api/auth") => Resp(handle_auth(req, ctx, now)),

        ("GET", "/api/logs") => {
            if !authed {
                return Resp(Response::unauthorized());
            }
            let Some(file) = req.query.get("file").and_then(|s| LogFile::from_param(s)) else {
                return Resp(Response::not_found());
            };
            let tail = req.query.get("tail").and_then(|s| s.parse::<usize>().ok());
            let lines = tail_snapshot(&today_path(&ctx.dirs, file, today), tail);
            Resp(Response::ok_json(
                serde_json::json!({ "lines": lines }).to_string(),
            ))
        }

        ("GET", "/api/stream") => {
            if !authed {
                return Resp(Response::unauthorized());
            }
            match req.query.get("file").and_then(|s| LogFile::from_param(s)) {
                Some(file) => Stream(file),
                None => Resp(Response::not_found()),
            }
        }

        ("GET", "/api/download") => {
            if !authed {
                return Resp(Response::unauthorized());
            }
            match build_zip(&ctx.dirs) {
                Ok(bytes) => Resp(Response::zip(bytes)),
                Err(_) => Resp(Response::json(
                    500,
                    "Internal Server Error",
                    r#"{"error":"zip_failed"}"#,
                )),
            }
        }

        ("POST", "/api/pause") => {
            if !authed {
                return Resp(Response::unauthorized());
            }
            (ctx.trigger_pause)();
            Resp(Response::ok_json(r#"{"ok":true}"#))
        }

        _ => Resp(Response::not_found()),
    }
}

#[derive(serde::Deserialize)]
struct AuthBody {
    pin: String,
}

/// Verify the pincode once and issue a session token. Consults `Lockout`
/// *before* hashing so a throttled gate never pays the argon2 cost.
fn handle_auth(req: &Request, ctx: &Ctx, now: Instant) -> Response {
    if let LockoutState::Locked { .. } = ctx.lockout.lock().unwrap().check(now) {
        return Response::json(429, "Too Many Requests", r#"{"error":"locked"}"#);
    }

    let Ok(body) = serde_json::from_slice::<AuthBody>(&req.body) else {
        return Response::json(400, "Bad Request", r#"{"error":"bad_body"}"#);
    };

    let cfg = crate::config::load_or_default(&ctx.cfg_path);
    // `verify` returns Err for a malformed / empty stored hash (no pincode
    // configured yet) — collapse that to "denied".
    let ok = crate::password::verify(&body.pin, &cfg.passcode_hash).unwrap_or(false);

    if ok {
        ctx.lockout.lock().unwrap().reset();
        let token = ctx.sessions.issue(now);
        Response::ok_json(r#"{"authed":true}"#).with_cookie("session", &token)
    } else {
        ctx.lockout.lock().unwrap().record_failure(now);
        Response::unauthorized()
    }
}

/// Build the download-all zip: one entry per log (`service.log` / `widget.log`
/// / `bench.log`), each the concatenation of that log's full on-disk history.
fn build_zip(dirs: &LogDirs) -> io::Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zw = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default();
        for file in LogFile::ALL {
            zw.start_file(file.base(), opts)?;
            for path in history_files(dirs, file) {
                if let Ok(bytes) = std::fs::read(&path) {
                    zw.write_all(&bytes)?;
                }
            }
        }
        zw.finish()?;
    }
    Ok(cursor.into_inner())
}

/// Bind `0.0.0.0:REMOTE_LOG_PORT` and serve thread-per-connection. Cross-
/// platform (std::net), so it compiles + could be smoke-tested on Linux, but
/// it is not spawned anywhere yet — session 2 calls this from the service.
pub fn server<F>(cfg_path: PathBuf, dirs: LogDirs, hostname: String, trigger_pause: F) -> io::Result<()>
where
    F: Fn() + Send + Sync + 'static,
{
    let ctx = Arc::new(Ctx::new(cfg_path, dirs, hostname, Box::new(trigger_pause)));
    let listener = TcpListener::bind(("0.0.0.0", REMOTE_LOG_PORT))?;
    tracing::info!(port = REMOTE_LOG_PORT, "remote log console listening");

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let ctx = Arc::clone(&ctx);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, &ctx) {
                        tracing::debug!(error = %e, "remote connection ended");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
    Ok(())
}

fn handle_connection(stream: TcpStream, ctx: &Ctx) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let req = parse_request(&mut reader)?;
    let mut writer = stream;

    let now = Instant::now();
    let today = Utc::now().date_naive();
    match route(&req, ctx, now, today) {
        RouteOutcome::Resp(resp) => resp.write_to(&mut writer)?,
        RouteOutcome::Stream(file) => stream_logs(ctx, file, today, &mut writer)?,
    }
    Ok(())
}

/// SSE handler: replay today's snapshot, then tail forever. Any write error
/// (client disconnect) ends the loop and the connection thread.
fn stream_logs(ctx: &Ctx, file: LogFile, today: NaiveDate, w: &mut TcpStream) -> io::Result<()> {
    write_sse_headers(w)?;

    for line in tail_snapshot(&today_path(&ctx.dirs, file, today), None) {
        w.write_all(sse_data(&line).as_bytes())?;
    }
    w.flush()?;

    let mut tailer = Tailer::new(&ctx.dirs, file, today);
    let mut idle_polls = 0u32;
    loop {
        thread::sleep(POLL_INTERVAL);
        let today = Utc::now().date_naive();
        let lines = tailer.poll(&ctx.dirs, today);
        if lines.is_empty() {
            idle_polls += 1;
            if idle_polls >= PING_EVERY_POLLS {
                w.write_all(SSE_PING.as_bytes())?;
                w.flush()?;
                idle_polls = 0;
            }
        } else {
            for line in lines {
                w.write_all(sse_data(&line).as_bytes())?;
            }
            w.flush()?;
            idle_polls = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn test_ctx(cfg_path: PathBuf, dirs: LogDirs, counter: Arc<AtomicUsize>) -> Ctx {
        Ctx::new(
            cfg_path,
            dirs,
            "child.local:8787".to_string(),
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    fn dirs() -> (tempfile::TempDir, LogDirs) {
        let tmp = tempfile::tempdir().unwrap();
        let diagnostics = tmp.path().join("diag");
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&diagnostics).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        (tmp, LogDirs { diagnostics, logs })
    }

    fn request(method: &str, path: &str, query: &[(&str, &str)], cookie: Option<&str>, body: &[u8]) -> Request {
        let mut headers = HashMap::new();
        if let Some(c) = cookie {
            headers.insert("cookie".to_string(), format!("session={c}"));
        }
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            headers,
            body: body.to_vec(),
        }
    }

    fn resp(outcome: RouteOutcome) -> Response {
        match outcome {
            RouteOutcome::Resp(r) => r,
            RouteOutcome::Stream(_) => panic!("expected a Response, got a Stream"),
        }
    }

    /// Write a `state.dat` with a known passcode hash (Linux crypto is
    /// identity, so `save` produces plain TOML that `load_or_default` reads).
    fn write_config_with_pin(cfg_path: &std::path::Path, pin: &str) {
        let cfg = crate::config::Config {
            passcode_hash: crate::password::hash(pin).unwrap(),
            ..Default::default()
        };
        crate::config::save(&cfg, cfg_path).unwrap();
    }

    #[test]
    fn root_serves_index_html() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let r = resp(route(&request("GET", "/", &[], None, b""), &ctx, Instant::now(), today()));
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("<!DOCTYPE html>"));
    }

    #[test]
    fn session_reports_unauthed_and_logs_list() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let r = resp(route(&request("GET", "/api/session", &[], None, b""), &ctx, Instant::now(), today()));
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["authed"], false);
        assert_eq!(v["hostname"], "child.local:8787");
        assert_eq!(v["logs"], serde_json::json!(["service", "widget", "bench"]));
    }

    #[test]
    fn logs_endpoint_requires_cookie() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let r = resp(route(
            &request("GET", "/api/logs", &[("file", "service")], None, b""),
            &ctx,
            Instant::now(),
            today(),
        ));
        assert_eq!(r.status, 401);
    }

    #[test]
    fn logs_endpoint_returns_lines_with_valid_cookie() {
        let (_t, d) = dirs();
        std::fs::write(today_path(&d, LogFile::Service, today()), "a\nb\nc\n").unwrap();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        let tok = ctx.sessions.issue(now);
        let r = resp(route(
            &request("GET", "/api/logs", &[("file", "service"), ("tail", "2")], Some(&tok), b""),
            &ctx,
            now,
            today(),
        ));
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["lines"], serde_json::json!(["b", "c"]));
    }

    #[test]
    fn logs_endpoint_unknown_file_is_404() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        let tok = ctx.sessions.issue(now);
        let r = resp(route(
            &request("GET", "/api/logs", &[("file", "install")], Some(&tok), b""),
            &ctx,
            now,
            today(),
        ));
        assert_eq!(r.status, 404);
    }

    #[test]
    fn auth_good_pin_issues_cookie_bad_pin_401() {
        let (_t, d) = dirs();
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("state.dat");
        write_config_with_pin(&cfg_path, "1234");
        let ctx = test_ctx(cfg_path, d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();

        let good = resp(route(
            &request("POST", "/api/auth", &[], None, br#"{"pin":"1234"}"#),
            &ctx,
            now,
            today(),
        ));
        assert_eq!(good.status, 200);
        assert!(good.headers.iter().any(|(k, _)| k == "Set-Cookie"));

        let bad = resp(route(
            &request("POST", "/api/auth", &[], None, br#"{"pin":"0000"}"#),
            &ctx,
            now,
            today(),
        ));
        assert_eq!(bad.status, 401);
        assert!(!bad.headers.iter().any(|(k, _)| k == "Set-Cookie"));
    }

    #[test]
    fn auth_locks_out_after_three_bad_pins() {
        let (_t, d) = dirs();
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("state.dat");
        write_config_with_pin(&cfg_path, "1234");
        let ctx = test_ctx(cfg_path, d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();

        for _ in 0..3 {
            let r = resp(route(
                &request("POST", "/api/auth", &[], None, br#"{"pin":"0000"}"#),
                &ctx,
                now,
                today(),
            ));
            assert_eq!(r.status, 401);
        }
        // 4th attempt is throttled (429) without hashing.
        let locked = resp(route(
            &request("POST", "/api/auth", &[], None, br#"{"pin":"1234"}"#),
            &ctx,
            now,
            today(),
        ));
        assert_eq!(locked.status, 429);
    }

    #[test]
    fn pause_fires_closure_only_when_authed() {
        let (_t, d) = dirs();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::clone(&counter));
        let now = Instant::now();

        let unauth = resp(route(&request("POST", "/api/pause", &[], None, b""), &ctx, now, today()));
        assert_eq!(unauth.status, 401);
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        let tok = ctx.sessions.issue(now);
        let ok = resp(route(&request("POST", "/api/pause", &[], Some(&tok), b""), &ctx, now, today()));
        assert_eq!(ok.status, 200);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn download_yields_zip_with_three_entries() {
        let (_t, d) = dirs();
        std::fs::write(today_path(&d, LogFile::Service, today()), "svc\n").unwrap();
        std::fs::write(today_path(&d, LogFile::Bench, today()), "bench\n").unwrap();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        let tok = ctx.sessions.issue(now);

        let r = resp(route(&request("GET", "/api/download", &[], Some(&tok), b""), &ctx, now, today()));
        assert_eq!(r.status, 200);

        let archive = zip::ZipArchive::new(Cursor::new(r.body)).unwrap();
        assert_eq!(archive.len(), 3);
    }

    #[test]
    fn stream_route_returns_stream_for_authed_valid_file() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        let tok = ctx.sessions.issue(now);

        let outcome = route(
            &request("GET", "/api/stream", &[("file", "widget")], Some(&tok), b""),
            &ctx,
            now,
            today(),
        );
        assert!(matches!(outcome, RouteOutcome::Stream(LogFile::Widget)));

        // Unauthed stream is a 401 Response, not a Stream.
        let unauth = route(
            &request("GET", "/api/stream", &[("file", "widget")], None, b""),
            &ctx,
            now,
            today(),
        );
        assert_eq!(resp(unauth).status, 401);
    }

    #[test]
    fn unknown_route_is_404() {
        let (_t, d) = dirs();
        let ctx = test_ctx(PathBuf::from("x"), d, Arc::new(AtomicUsize::new(0)));
        let r = resp(route(&request("GET", "/nope", &[], None, b""), &ctx, Instant::now(), today()));
        assert_eq!(r.status, 404);
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 25).unwrap()
    }
}
