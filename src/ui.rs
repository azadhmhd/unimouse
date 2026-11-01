//! Settings UI: a local web page served by the running unimouse, so it looks and works the same
//! on every OS with no GUI toolkit. Saving writes config.toml; the engine hot-reloads it on the next click.
//!
//! Security: the page can rewrite the config, and `run:` actions execute commands, so a random website
//! must never reach it. Three guards: loopback-only socket, a per-run random token on every API call
//! (stops cross-site requests), and an exact Host check (stops DNS rebinding).

use crate::{
    engine::{launch, Action, Raw, INPUT_ACTIVE, MONITOR},
    install,
};
use serde_json::{json, Value};
use std::{
    collections::hash_map::RandomState,
    collections::HashMap,
    fs,
    hash::{BuildHasher, Hasher},
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

const PAGE: &str = include_str!("ui.html");
pub const PORT: u16 = 47613;
const MAX_BODY: usize = 1 << 20;

/// Starts the server thread. Returns the page URL (with its token).
pub fn start(config: PathBuf, port: u16) -> io::Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let host = format!("127.0.0.1:{}", listener.local_addr()?.port());
    let token = random_token();
    let url = format!("http://{host}/?token={token}");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Err(e) = handle(stream, &config, &host, &token) {
                eprintln!("settings: {e}")
            }
        }
    });
    Ok(url)
}

/// Where a running copy leaves its URL, so `unimouse --settings` can open it.
pub fn url_file(config: &Path) -> PathBuf {
    config.with_file_name("settings-url")
}

pub fn write_url_file(config: &Path, url: &str) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600); // holds the token
    opts.open(url_file(config))?.write_all(url.as_bytes())
}

// ponytail: 128 bits from std's OS-seeded hasher keys; reach for getrandom if this ever guards more than a local page.
fn random_token() -> String {
    let part = || {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(now_nanos());
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn handle(stream: TcpStream, config: &Path, host: &str, token: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream.take((MAX_BODY + 64 * 1024) as u64));

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let host_ok = headers.get("host").is_some_and(|h| h == host || *h == host.replace("127.0.0.1", "localhost"));
    if !host_ok {
        return respond(&mut out, 403, "text/plain", "forbidden");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=')).unwrap_or("");

    if method == "GET" && path == "/" {
        return respond(&mut out, 200, "text/html; charset=utf-8", PAGE);
    }
    if !path.starts_with("/api/") {
        return respond(&mut out, 404, "text/plain", "not found");
    }
    if param("token") != token {
        return respond(&mut out, 403, "text/plain", "forbidden");
    }

    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return respond(&mut out, 413, "text/plain", "too large");
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;

    let (status, value) = api(method, path, param("since"), &body, config);
    respond(&mut out, status, "application/json", &value.to_string())
}

fn api(method: &str, path: &str, since: &str, body: &[u8], config: &Path) -> (u16, Value) {
    let bad = |e: String| (400, json!({ "error": e }));
    match (method, path) {
        ("GET", "/api/info") => (
            200,
            json!({
                "os": std::env::consts::OS,
                "config": config.display().to_string(),
                "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
                "start_at_login": install::is_installed(),
            }),
        ),
        ("GET", "/api/config") => {
            match fs::read_to_string(config).map_err(|e| e.to_string()).and_then(|t| Raw::parse(&t)) {
                Ok(raw) => (200, json!(raw)),
                Err(e) => bad(e),
            }
        }
        ("POST", "/api/config") => {
            let saved = serde_json::from_slice::<Raw>(body)
                .map_err(|e| e.to_string())
                .and_then(|raw| raw.to_toml())
                .and_then(|text| fs::write(config, text).map_err(|e| e.to_string()));
            match saved {
                Ok(()) => (200, json!({ "ok": true })),
                Err(e) => bad(e),
            }
        }
        // Presses and fired actions after `since`. Polling this also asks the engine to note app names for a while.
        ("GET", "/api/events") => {
            let since: u64 = since.parse().unwrap_or(0);
            let mut m = MONITOR.lock().unwrap();
            m.watch_until = Some(Instant::now() + Duration::from_secs(5));
            let events: Vec<_> = m.events.iter().filter(|e| e.id > since).cloned().collect();
            (200, json!(events))
        }
        // Practice mode: actions are reported but not performed. The UI renews it while it's on; it lapses on its own.
        ("POST", "/api/practice") => {
            let on = flag(body);
            MONITOR.lock().unwrap().practice_until = on.then(|| Instant::now() + Duration::from_secs(90));
            (200, json!({ "ok": true }))
        }
        ("GET", "/api/health") => (
            200,
            json!({
                "input_active": INPUT_ACTIVE.load(Ordering::Relaxed),
                "conflicts": conflicts(),
                "mice": crate::platform::mice(),
            }),
        ),
        // Fixed destinations only: the page never gets to open arbitrary paths or URLs.
        ("POST", "/api/open") => {
            let what = serde_json::from_slice::<Value>(body).ok().and_then(|v| v["what"].as_str().map(str::to_string));
            let target = match what.as_deref() {
                Some("accessibility") if cfg!(target_os = "macos") => {
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility".to_string()
                }
                Some("config-folder") => config.parent().map(|p| p.display().to_string()).unwrap_or_default(),
                _ => return (404, json!({ "error": "unknown target" })),
            };
            launch(&Action::Open(target));
            (200, json!({ "ok": true }))
        }
        // "Detect a button": capture presses for up to 30 s instead of acting on them.
        ("POST", "/api/learn") => {
            let on = flag(body);
            MONITOR.lock().unwrap().learn_until = on.then(|| Instant::now() + Duration::from_secs(30));
            (200, json!({ "ok": true }))
        }
        ("POST", "/api/start-at-login") => {
            let on = flag(body);
            let result = if on { install::install(config) } else { install::uninstall() };
            match result {
                Ok(msg) => (200, json!({ "ok": true, "message": msg })),
                Err(e) => bad(e),
            }
        }
        _ => (404, json!({ "error": "not found" })),
    }
}

/// `{"on": true}` bodies.
fn flag(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).ok().and_then(|v| v["on"].as_bool()).unwrap_or(false)
}

/// Other mouse tools that grab the same buttons, by process name, with the name people know them by.
const CONFLICTS: [(&str, &str); 12] = [
    ("logioptionsplus_agent", "Logi Options+"),
    ("LogiMgrDaemon", "Logitech Options"),
    ("lghub_agent", "Logitech G HUB"),
    ("Mac Mouse Fix Helper", "Mac Mouse Fix"),
    ("SteerMouse", "SteerMouse"),
    ("BetterTouchTool", "BetterTouchTool"),
    ("LinearMouse", "LinearMouse"),
    ("XMouseButtonControl", "X-Mouse Button Control"),
    ("RazerAppEngine", "Razer Synapse"),
    ("solaar", "Solaar"),
    ("input-remapper", "Input Remapper"),
    ("piper", "Piper"),
];

fn conflicts() -> Vec<&'static str> {
    #[cfg(windows)]
    let out = Command::new("tasklist").args(["/fo", "csv", "/nh"]).output();
    #[cfg(not(windows))]
    let out = Command::new("ps").args(["-axo", "comm="]).output();
    let list = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    CONFLICTS.iter().filter(|(proc_name, _)| list.contains(proc_name)).map(|(_, name)| *name).collect()
}

fn respond(out: &mut TcpStream, status: u16, content_type: &str, body: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        413 => "Payload Too Large",
        _ => "Not Found",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EXAMPLE_CONFIG;

    fn request(addr: &str, raw: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn guards_and_saving() {
        let dir = std::env::temp_dir().join(format!("unimouse-ui-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(&config, EXAMPLE_CONFIG).unwrap();
        let url = start(config.clone(), 0).unwrap();
        let addr = url.trim_start_matches("http://").split('/').next().unwrap().to_string();
        let token = url.split("token=").nth(1).unwrap().to_string();
        let get = |path: &str, host: &str| request(&addr, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n"));

        assert!(get("/", &addr).starts_with("HTTP/1.1 200"));
        assert!(get("/api/config", &addr).starts_with("HTTP/1.1 403"), "no token");
        assert!(get("/api/config?token=wrong", &addr).starts_with("HTTP/1.1 403"), "bad token");
        assert!(
            get(&format!("/api/config?token={token}"), "evil.example:80").starts_with("HTTP/1.1 403"),
            "DNS rebinding"
        );
        let ok = get(&format!("/api/config?token={token}"), &addr);
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("\"button4\":\"back\""));

        let post = |body: &str| {
            request(&addr, &format!(
            "POST /api/config?token={token} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
        };
        let before = fs::read_to_string(&config).unwrap();
        assert!(post(r#"{"default":{"button4":"teleport"}}"#).starts_with("HTTP/1.1 400"));
        assert_eq!(fs::read_to_string(&config).unwrap(), before, "rejected config must not be written");
        assert!(post(r#"{"default":{"button4":"keys:mod+c"},"app":{"safari":{"button5":"mute"}}}"#)
            .starts_with("HTTP/1.1 200"));
        let saved = Raw::parse(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(saved.default["button4"], "keys:mod+c");
        assert_eq!(saved.app["safari"]["button5"], "mute");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Look at the settings page without Accessibility access or a real mouse:
    /// `cargo test preview -- --ignored --nocapture`, then open the printed URL.
    /// Uses a throwaway config and pretends an extra button (6) is pressed every few seconds.
    #[test]
    #[ignore]
    fn preview() {
        let dir = std::env::temp_dir().join("unimouse-preview");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(&config, EXAMPLE_CONFIG).unwrap();
        let url = start(config, PORT + 1).unwrap();
        println!("preview: {url}");
        fs::write(dir.join("url"), &url).unwrap();
        for i in 0.. {
            thread::sleep(Duration::from_secs(4));
            MONITOR
                .lock()
                .unwrap()
                .record(if i % 3 == 2 { 4 } else { 6 }, if i % 2 == 0 { "Safari".into() } else { "Code".into() });
            INPUT_ACTIVE.store(true, Ordering::Relaxed);
            if i > 450 {
                break;
            } // ~30 minutes
        }
    }
}
