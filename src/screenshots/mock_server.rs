//! Loopback HTTP/SSE server that serves a fixed fixture to the GUI's
//! `GatewayHttpClient`. No gateway process, database or credential is involved:
//! the GUI opens a real `GatewayRuntime` pointed at a fixed loopback port
//! (`127.0.0.1:18787`, so the status bar stays byte-stable across runs) and
//! every admin read it makes is answered from the fixture map below.

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// A path → JSON body map plus the first SSE frame pushed on `/api/activity`.
pub(crate) struct MockRoutes {
    pub bodies: HashMap<String, String>,
    /// The `data:` payload of the first activity frame (a serialized
    /// `ActivityEvent::Bootstrap`). Sent once, then the stream is held open.
    pub bootstrap_event: String,
}

/// A loopback fixture server. Dropping it stops the accept loop.
pub(crate) struct MockServer {
    base: String,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// A fixed loopback port keeps the GUI's status-bar gateway URL identical
/// across runs (the whole point is byte-stable PNG baselines). Binding a
/// fixed port fails loudly rather than silently changing the picture.
const MOCK_PORT: u16 = 18787;

impl MockServer {
    pub(crate) fn start(routes: MockRoutes) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", MOCK_PORT))
            .map_err(|e| format!("bind mock server on 127.0.0.1:{MOCK_PORT}: {e}"))?;
        let addr = listener.local_addr().expect("mock server local addr");
        listener
            .set_nonblocking(true)
            .expect("set mock listener nonblocking");
        let base = format!("http://127.0.0.1:{}", addr.port());
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (hits, seen, stop) = (hits.clone(), seen.clone(), stop.clone());
            thread::spawn(move || {
                let routes = Arc::new(routes);
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let routes = routes.clone();
                            let hits = hits.clone();
                            let seen = seen.clone();
                            thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                                serve(&mut stream, &routes, &hits, &seen);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Ok(Self {
            base,
            hits,
            seen,
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    /// Number of requests served so far — reported at the end of a run as a
    /// diagnostic aid when a scene renders empty.
    pub(crate) fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// Path → hit list, for diagnostics when a scene renders empty.
    pub(crate) fn seen_paths(&self) -> Vec<String> {
        self.seen.lock().expect("mock seen lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    stream: &mut std::net::TcpStream,
    routes: &MockRoutes,
    hits: &AtomicUsize,
    seen: &Mutex<Vec<String>>,
) {
    // Read until the full header block arrives. TCP may split a single GET
    // across segments; a one-shot read can see only part of the request line,
    // turning /health into an empty path -> 404 -> a permanently Unreachable
    // gateway link and every later admin call silently dropped.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() >= 8192 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let request_line = request.lines().next().unwrap_or("");
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/").to_owned();
    hits.fetch_add(1, Ordering::Relaxed);
    seen.lock().expect("mock seen lock").push(path.clone());

    if path == "/api/activity" {
        serve_sse(stream, &routes.bootstrap_event);
        return;
    }

    let (status, content_type, body) = match routes.bodies.get(&path) {
        Some(body) => ("200 OK", "application/json; charset=utf-8", body.clone()),
        None => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            format!("no mock for {path}"),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn serve_sse(stream: &mut std::net::TcpStream, bootstrap_event: &str) {
    // SSE is long-lived: the generic per-connection read timeout would close
    // it and trigger an endless reconnect/reseed loop in the GUI.
    let _ = stream.set_read_timeout(None);
    let header = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-cache\r\n",
        "Connection: keep-alive\r\n",
        "\r\n",
    );
    let frame = format!("data: {bootstrap_event}\n\n");
    if stream.write_all(header.as_bytes()).is_err() || stream.write_all(frame.as_bytes()).is_err() {
        return;
    }
    if stream.flush().is_err() {
        return;
    }
    // Hold the stream open without a timer: block until the client closes.
    // Reconnecting here would re-push Bootstrap and make renders non-deterministic.
    let mut sink = [0u8; 512];
    while let Ok(n) = stream.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
}
