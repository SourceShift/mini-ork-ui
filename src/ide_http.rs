//! The IDE's fast path to mini-ork: a `mini-ork serve` spoken to over loopback.
//!
//! Reading the board otherwise means running `mini-ork board --json` as a
//! subprocess — a `/bin/sh` launcher, the dispatcher Python, and a second
//! `python -m` for the module, ~0.25 s of interpreter startup before any of
//! mini-ork's own work, paid again on every poll and every click. The same
//! payloads are served from a process that is already warm (`web/routes/ide.py`
//! runs the identical `board_cmd` code), so this module asks that server first
//! and the subprocess stays the fallback.
//!
//! A server is started lazily by the first read that finds none and left
//! running for the session — like a language server, and because the state that
//! makes it worth having (mini-ork's imports, its DB handles) is exactly the
//! state that a per-read process would have to rebuild. If it cannot start —
//! no `fastapi`/`uvicorn` in that checkout, no `state.db`, the volume it lives
//! on unreadable — `get` returns `None` and every read falls back, so a missing
//! server costs the IDE the speedup and nothing else.
//!
//! The port is where this used to go wrong. `7090` may already be held by a
//! `serve` someone else started — an older build, a checkout that has since
//! moved, a server for a different project — and that server answers, so the
//! read looked served while the server was in fact unable to render the page
//! (`ok: false`, or a 503 for sources edited under it). The IDE then fell back
//! to a subprocess, which on a volume the app is not permitted to read fails
//! with a Python startup traceback. A server that answers is therefore not
//! enough: this module adopts one only when it serves the read it was asked
//! for, and otherwise starts its own on a free port, which `7090` cannot deny.

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};

use futures::AsyncReadExt as _;
use gpui::BackgroundExecutor;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Request, StatusCode, Url};
use serde_json::Value;
use util::command::Stdio;

use crate::board::IDE_SPEC;

/// The port `mini-ork serve` binds by default. Fixed rather than negotiated
/// because a server the user already started by hand is worth reusing, and
/// because a fixed port is what the first probe can look at.
const DEFAULT_PORT: u16 = 7090;
/// How long to wait for a server this process started to answer. `mini-ork
/// serve` imports fastapi and uvicorn, which is the slow part.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to ask a starting server whether it is up yet.
const START_POLL: Duration = Duration::from_millis(100);
/// A read that hangs would be worse than the subprocess it replaced.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The port this session reads from; `0` until a server is adopted or started.
static PORT: AtomicU16 = AtomicU16::new(0);
/// Whether the server on `PORT` is one this process started. A server someone
/// else started is shared and may belong to another project, so a page it
/// refuses is a reason to get our own; our own server has no other project to
/// confuse it with, and a page it refuses is that page's own answer.
static OWNED: AtomicBool = AtomicBool::new(false);
/// Set once no usable server could be had, so every read takes the subprocess
/// path without paying another probe or spawning servers on a loop.
static GAVE_UP: AtomicBool = AtomicBool::new(false);

/// The app's HTTP client and executor, handed over once by `store::init`. Until
/// then no request is made and every read takes the subprocess path.
static HTTP: OnceLock<Arc<dyn HttpClient>> = OnceLock::new();
static EXECUTOR: OnceLock<BackgroundExecutor> = OnceLock::new();
/// Serialises choosing a server, so concurrent first reads do not each start
/// one. `futures`' own mutex needs no runtime to drive it.
static CHOOSING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());
/// The home this process last confirmed the server on a port serves. Checked
/// once per (port, home), since the answer cannot change while the same
/// process owns the port.
static HOME_CHECK: Mutex<Option<(u16, PathBuf, bool)>> = Mutex::new(None);

pub fn init(client: Arc<dyn HttpClient>, executor: BackgroundExecutor) {
    HTTP.set(client).ok();
    EXECUTOR.set(executor).ok();
}

/// What one probe of a server found.
enum Probe {
    /// A payload this read can use.
    Payload(Value),
    /// The server answered, but not with this read — a 4xx/5xx, a body that is
    /// not JSON, or an `ok: false` payload. It is plainly up, so it must not be
    /// mistaken for a server that is missing and restarted.
    Rejected,
    /// Nothing is listening.
    Offline,
}

/// What waiting for a server this process started found.
enum Waited {
    /// The server answered this read.
    Ready(Value),
    /// The server is up but answered `ok: false`. That is the page's own
    /// refusal — an unknown tab, a run that no longer exists — and the caller
    /// renders it as the page's error. `None` is a refusal with no payload to
    /// show (a 4xx/5xx, a body that is not JSON).
    Refused(Option<Value>),
    /// The server never answered.
    Timeout,
}

/// One read over loopback, or `None` to mean "take the subprocess path".
///
/// `bin` is the mini-ork launcher (started if nothing usable answers) and `home`
/// the project to read — sent as the `home` query parameter, which is how one
/// server serves whichever project the IDE has open.
pub async fn get(bin: &Path, home: &Path, path: &str, query: &[(&str, String)]) -> Option<Value> {
    if HTTP.get().is_none() {
        return None;
    }
    // The server this session settled on.
    if let Some(port) = chosen_port() {
        match probe(port, home, path, query).await {
            // Our own server refusing a read is the read's own failure, and
            // its message is the one to show; starting another server would
            // refuse it too. A shared server that refuses may be another
            // project's, so it is not trusted with the answer.
            Probe::Payload(value) => {
                if OWNED.load(Ordering::Relaxed) || usable(&value) {
                    return Some(value);
                }
                PORT.store(0, Ordering::Relaxed);
            }
            // A non-2xx from our own server is this read's failure. From a
            // shared one — a 503 for edited sources, a 404 from an older
            // build — it means the server is not this project's.
            Probe::Rejected => {
                if OWNED.load(Ordering::Relaxed) {
                    return None;
                }
                PORT.store(0, Ordering::Relaxed);
            }
            Probe::Offline => {
                if OWNED.load(Ordering::Relaxed) {
                    // A server this process started and that has since died:
                    // fall back for the session rather than respawn on every
                    // read.
                    GAVE_UP.store(true, Ordering::Relaxed);
                }
                PORT.store(0, Ordering::Relaxed);
            }
        }
    }
    if GAVE_UP.load(Ordering::Relaxed) {
        return None;
    }
    // Reuse a server already listening on the default port, but only one that
    // serves this read and resolves this home as its active project: a server
    // that per-request homes predate answers with its own project's data.
    if chosen_port().is_none() {
        if let Probe::Payload(value) = probe(DEFAULT_PORT, home, path, query).await {
            if usable(&value) && home_is_served(DEFAULT_PORT, home).await {
                PORT.store(DEFAULT_PORT, Ordering::Relaxed);
                return Some(value);
            }
        }
    }
    // Everything else starts our own server, one reader at a time.
    let _choosing = CHOOSING.lock().await;
    // Another read may have chosen while this one waited for the lock.
    if let Some(port) = chosen_port() {
        return match wait_for_server(port, home, path, query, None).await {
            Waited::Ready(value) | Waited::Refused(Some(value)) => Some(value),
            Waited::Refused(None) | Waited::Timeout => None,
        };
    }
    if GAVE_UP.load(Ordering::Relaxed) {
        return None;
    }
    let Some(port) = free_port() else {
        GAVE_UP.store(true, Ordering::Relaxed);
        return None;
    };
    PORT.store(port, Ordering::Relaxed);
    OWNED.store(true, Ordering::Relaxed);
    let Ok(mut child) = start(bin, home, port) else {
        GAVE_UP.store(true, Ordering::Relaxed);
        PORT.store(0, Ordering::Relaxed);
        return None;
    };
    match wait_for_server(port, home, path, query, Some(&mut child)).await {
        Waited::Ready(value) | Waited::Refused(Some(value)) => Some(value),
        Waited::Refused(None) => None,
        Waited::Timeout => {
            GAVE_UP.store(true, Ordering::Relaxed);
            PORT.store(0, Ordering::Relaxed);
            None
        }
    }
}

/// The port chosen this session, if any.
fn chosen_port() -> Option<u16> {
    match PORT.load(Ordering::Relaxed) {
        0 => None,
        port => Some(port),
    }
}

/// A port nothing is listening on, for a server this process starts. Binding
/// and closing is the standard way to have the kernel pick one free port; a
/// server that loses the race to another process simply never answers, and the
/// read falls back.
fn free_port() -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// Whether a payload is this server's answer rather than its refusal. An
/// `ok: false` payload is a well-formed page failure (an unknown tab, a broken
/// run) and must reach the caller as such, not be retried elsewhere.
fn usable(value: &Value) -> bool {
    value.get("ok").and_then(Value::as_bool) != Some(false)
}

/// Wait for a server this process started to answer, up to `START_TIMEOUT`.
///
/// `child` is the launcher when this process spawned it: a server that dies on
/// startup — Python denied the read of its own checkout, no `fastapi` — must be
/// reported now rather than after the full timeout, because every read until
/// then waits for a server that is already gone.
async fn wait_for_server(
    port: u16,
    home: &Path,
    path: &str,
    query: &[(&str, String)],
    mut child: Option<&mut util::command::Child>,
) -> Waited {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match probe(port, home, path, query).await {
            Probe::Payload(value) if usable(&value) => return Waited::Ready(value),
            Probe::Payload(value) => return Waited::Refused(Some(value)),
            Probe::Rejected => return Waited::Refused(None),
            Probe::Offline => {}
        }
        if child
            .as_mut()
            .is_some_and(|child| matches!(child.try_status(), Ok(Some(_))))
        {
            return Waited::Timeout;
        }
        if Instant::now() >= deadline {
            return Waited::Timeout;
        }
        // No scheduler means no way to wait; one probe is all this read gets.
        match EXECUTOR.get() {
            Some(executor) => executor.timer(START_POLL).await,
            None => return Waited::Timeout,
        }
    }
}

/// Whether the server on `port` resolves `home` as its active project. The
/// server is asked for its own view (`/api/v1/projects`) rather than trusted to
/// have honoured the `home` parameter on the read that got us here: a server
/// started before per-request homes existed answers with its own project and
/// looks perfectly healthy doing it.
async fn home_is_served(port: u16, home: &Path) -> bool {
    let checked = HOME_CHECK.lock().ok().and_then(|slot| slot.clone());
    if let Some((checked_port, cached, served)) = checked {
        if checked_port == port && cached == home {
            return served;
        }
    }
    let Some(client) = HTTP.get() else {
        return false;
    };
    let Ok(mut url) = Url::parse(&format!("http://127.0.0.1:{port}/api/v1/projects")) else {
        return false;
    };
    url.query_pairs_mut()
        .append_pair("home", &home.display().to_string());
    let Ok(request) = Request::get(url.as_str())
        .timeout(REQUEST_TIMEOUT)
        .body(AsyncBody::default())
    else {
        return false;
    };
    let Ok(mut response) = client.send(request).await else {
        return false;
    };
    if response.status() != StatusCode::OK {
        return false;
    }
    let mut body = Vec::new();
    if response.body_mut().read_to_end(&mut body).await.is_err() {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return false;
    };
    let active = value
        .get("active")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let served = same_path(active, home);
    if let Ok(mut slot) = HOME_CHECK.lock() {
        *slot = Some((port, home.to_path_buf(), served));
    }
    served
}

/// Compare a path the server reported with the one this process asked for.
/// Both are canonicalised because macOS reports `/private/tmp` for a `/tmp`
/// home, and a symlinked project directory is the same project.
fn same_path(reported: &str, home: &Path) -> bool {
    if reported.is_empty() {
        return false;
    }
    let reported = Path::new(reported);
    match (reported.canonicalize(), home.canonicalize()) {
        (Ok(reported), Ok(home)) => reported == home,
        _ => reported == home,
    }
}

async fn probe(port: u16, home: &Path, path: &str, query: &[(&str, String)]) -> Probe {
    let Some(client) = HTTP.get() else {
        return Probe::Offline;
    };
    let Ok(mut url) = Url::parse(&format!("http://127.0.0.1:{port}{path}")) else {
        return Probe::Rejected;
    };
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("home", &home.display().to_string());
        // The spec level travels per request, not only on a server we started:
        // one already listening was started by something else — the user, an
        // earlier session — and its own default is level 1, the retired page.
        pairs.append_pair("spec", IDE_SPEC);
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
    }
    let Ok(request) = Request::get(url.as_str())
        .timeout(REQUEST_TIMEOUT)
        .body(AsyncBody::default())
    else {
        return Probe::Rejected;
    };
    // A refused connection is the one outcome that means "no server"; a reply
    // of any kind means there is one.
    let Ok(mut response) = client.send(request).await else {
        return Probe::Offline;
    };
    if response.status() != StatusCode::OK {
        return Probe::Rejected;
    }
    let mut body = Vec::new();
    if response.body_mut().read_to_end(&mut body).await.is_err() {
        return Probe::Rejected;
    }
    match serde_json::from_slice(&body) {
        Ok(value) => Probe::Payload(value),
        Err(_) => Probe::Rejected,
    }
}

/// Start `mini-ork serve` for this home and let it outlive the IDE. Detached: no
/// pipes to inherit, and no kill when the handle drops, since the whole point is
/// that it stays up between reads.
fn start(bin: &Path, home: &Path, port: u16) -> std::io::Result<util::command::Child> {
    let mut command = util::command::new_command(bin);
    command
        .args([
            "serve".to_string(),
            "--port".to_string(),
            port.to_string(),
            "--home".to_string(),
        ])
        .arg(home)
        .env("MINI_ORK_HOME", home)
        .env("MINI_ORK_IDE_SPEC", IDE_SPEC)
        // Let the launcher re-exec into mini-ork's own Python; an inherited
        // flag would stop it.
        .env_remove("MINI_ORK_VENV_ACTIVE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    if let Some(project) = home.parent() {
        command.current_dir(project);
    }
    command.spawn().inspect_err(|error| {
        log::warn!("mini-ork: could not start the IDE server: {error:#}");
    })
}

#[cfg(test)]
mod tests {
    use super::{free_port, same_path, usable};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn an_empty_reported_home_never_matches() {
        assert!(!same_path("", Path::new("/tmp")));
    }

    #[test]
    fn only_the_same_directory_matches() {
        assert!(same_path("/tmp", Path::new("/tmp")));
        assert!(!same_path("/var", Path::new("/tmp")));
    }

    #[test]
    fn a_symlinked_home_matches_its_target() {
        // macOS reports /private/tmp for a /tmp home, and a project directory
        // reached through a symlink is the same project. Both spellings must
        // compare equal, or every read on that home falls back to a subprocess.
        let Ok(canonical) = std::fs::canonicalize("/tmp") else {
            return;
        };
        assert!(same_path(&canonical.to_string_lossy(), Path::new("/tmp")));
    }

    #[test]
    fn a_refusal_is_not_a_usable_payload() {
        // `ok: false` is the server saying it cannot render this page (an
        // unknown tab, a broken run). It must not be mistaken for a healthy
        // answer, or the IDE would show the refusal as page content.
        assert!(usable(&json!({"ok": true, "tabs": []})));
        assert!(usable(&json!({"runs": []})));
        assert!(!usable(&json!({"ok": false, "error": "no such tab"})));
    }

    #[test]
    fn a_free_port_is_bindable() {
        let Some(port) = free_port() else {
            return;
        };
        assert!(port > 0);
        // The port was released, so a server can take it.
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
