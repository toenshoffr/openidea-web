//! The IDE in a browser: `openidea-server --listen`.
//!
//! One TCP port serves the built frontend (static files) and, at `<prefix>/ws`, a WebSocket per
//! browser tab. The socket carries the remote-development protocol of [`crate::rpc`], one JSON
//! message per text frame, so a tab is a client like the desktop app over SSH.
//!
//! The listener is a code-execution surface (terminals, run configurations), so access needs a
//! token by default and every WebSocket must come from the page's own origin (a page on another
//! port of the same machine could otherwise ride the cookie).

use futures::{SinkExt, StreamExt};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const COOKIE: &str = "openidea_token";
const MAX_HEAD: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Proxies close idle connections after about a minute.
const KEEPALIVE: Duration = Duration::from_secs(20);
const CSP: &str = "default-src 'self'; img-src 'self' data: blob: http: https:; style-src 'self' 'unsafe-inline'; \
    font-src 'self' data:; connect-src 'self'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self' blob:; \
    frame-ancestors 'none'";

pub struct WebConfig {
    /// The built frontend (`dist/`).
    pub web_root: PathBuf,
    /// The secret a visitor presents once (`?token=`) and keeps as a cookie. `None`: the operator
    /// has put authentication in front (a workspace proxy) and asked for none here.
    pub token: Option<String>,
}

/// Serves one connected tab over `stream` (the server end of an in-memory pipe) until it closes.
pub type SessionFn = Arc<dyn Fn(DuplexStream) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub async fn serve(listener: TcpListener, cfg: Arc<WebConfig>, session: SessionFn) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let (cfg, session) = (cfg.clone(), session.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(stream, cfg, session).await {
                tracing::debug!("connection: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn is_websocket(&self) -> bool {
        self.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    }

    fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| percent_decode(v))
        })
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == name).then_some(v)
        })
    }
}

fn parse_head(head: &[u8]) -> Option<Request> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(head), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    let target = req.path?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some(Request {
        method: req.method?.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        headers: req
            .headers
            .iter()
            .map(|h| (h.name.to_owned(), String::from_utf8_lossy(h.value).into_owned()))
            .collect(),
    })
}

/// Reads up to the end of the request head; what was read past it stays in the returned buffer.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::other("request head too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn handle(mut stream: TcpStream, cfg: Arc<WebConfig>, session: SessionFn) -> std::io::Result<()> {
    let head = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream))
        .await
        .map_err(|_| std::io::Error::other("request head timed out"))??;
    let Some(req) = parse_head(&head) else {
        return respond(&mut stream, 400, "text/plain", b"bad request", &[], false).await;
    };
    if req.is_websocket() && req.path.ends_with("/ws") {
        return websocket(stream, head, req, cfg, session).await;
    }
    static_file(&mut stream, req, &cfg).await
}

enum Access {
    Granted { set_cookie: bool },
    Denied(u16, &'static str),
}

fn check_access(req: &Request, cfg: &WebConfig, websocket: bool) -> Access {
    // The page and the socket come from the same origin; behind a proxy the original host may
    // arrive as `X-Forwarded-Host`.
    if let Some(origin) = req.header("origin") {
        let host = origin.split_once("://").map_or(origin, |(_, h)| h);
        let same = [req.header("host"), req.header("x-forwarded-host")]
            .into_iter()
            .flatten()
            .any(|h| h.eq_ignore_ascii_case(host));
        if !same && (websocket || req.method != "GET") {
            return Access::Denied(403, "cross-origin request refused");
        }
    }
    let Some(token) = &cfg.token else { return Access::Granted { set_cookie: false } };
    if let Some(given) = req.query_param("token")
        && !websocket
        && constant_time_eq(given.as_bytes(), token.as_bytes())
    {
        return Access::Granted { set_cookie: true };
    }
    if req.cookie(COOKIE).is_some_and(|c| constant_time_eq(c.as_bytes(), token.as_bytes())) {
        return Access::Granted { set_cookie: false };
    }
    Access::Denied(401, "OpenIDEA needs its access token: open the URL the server printed at start-up")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, String)],
    head_only: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCache-Control: no-cache\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

async fn static_file(stream: &mut TcpStream, req: Request, cfg: &WebConfig) -> std::io::Result<()> {
    let head_only = req.method == "HEAD";
    if req.method != "GET" && !head_only {
        return respond(stream, 405, "text/plain", b"method not allowed", &[("Allow", "GET, HEAD".into())], false)
            .await;
    }
    let set_cookie = match check_access(&req, cfg, false) {
        Access::Granted { set_cookie } => set_cookie,
        Access::Denied(status, why) => {
            return respond(stream, status, "text/plain", why.as_bytes(), &[], head_only).await;
        }
    };
    let mut cookie_header = Vec::new();
    if set_cookie && let Some(token) = &cfg.token {
        cookie_header.push(("Set-Cookie", format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict")));
        // Drop the token from the address bar (history, referrers): the cookie carries it now.
        let last = req.path.rsplit('/').next().unwrap_or("");
        let mut headers = cookie_header;
        headers.push(("Location", format!("./{last}")));
        return respond(stream, 302, "text/plain", b"", &headers, head_only).await;
    }
    let Some(rel) = safe_relative(&req.path) else {
        return respond(stream, 404, "text/plain", b"not found", &[], head_only).await;
    };
    let file = cfg.web_root.join(&rel);
    let Ok(file) = tokio::fs::canonicalize(&file).await else {
        return respond(stream, 404, "text/plain", b"not found", &[], head_only).await;
    };
    let root = tokio::fs::canonicalize(&cfg.web_root).await.unwrap_or_else(|_| cfg.web_root.clone());
    if !file.starts_with(&root) {
        return respond(stream, 404, "text/plain", b"not found", &[], head_only).await;
    }
    let Ok(mut body) = tokio::fs::read(&file).await else {
        return respond(stream, 404, "text/plain", b"not found", &[], head_only).await;
    };
    let kind = content_type(&file);
    let mut extra = Vec::new();
    if kind.starts_with("text/html") {
        body = mark_as_web(&body);
        extra.push(("Content-Security-Policy", CSP.to_owned()));
        extra.push(("X-Frame-Options", "DENY".to_owned()));
    }
    respond(stream, 200, kind, &body, &extra, head_only).await
}

/// `index.html` tells the app it is served by `openidea-server` (and not by a dev server).
fn mark_as_web(html: &[u8]) -> Vec<u8> {
    const MARK: &str = r#"<meta name="openidea-web" content="1" />"#;
    let text = String::from_utf8_lossy(html);
    match text.find("<head>") {
        Some(i) => {
            let at = i + "<head>".len();
            format!("{}{MARK}{}", &text[..at], &text[at..]).into_bytes()
        }
        None => html.to_vec(),
    }
}

/// The request path as a path below the web root: `/` and directories mean their `index.html`;
/// anything that climbs out is refused.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(path);
    if decoded.contains(['\0', '\\']) {
        return None;
    }
    let mut out = PathBuf::new();
    for part in decoded.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            return None;
        }
        out.push(part);
    }
    if decoded.ends_with('/') || out.as_os_str().is_empty() {
        out.push("index.html");
    }
    Some(out)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn content_type(file: &std::path::Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A stream that first replays bytes already read from it.
struct Replay {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl AsyncRead for Replay {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let pos = self.prefix.position() as usize;
        let rest = &self.prefix.get_ref()[pos..];
        if !rest.is_empty() {
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            self.prefix.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Replay {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn websocket(
    mut stream: TcpStream,
    head: Vec<u8>,
    req: Request,
    cfg: Arc<WebConfig>,
    session: SessionFn,
) -> std::io::Result<()> {
    if let Access::Denied(status, why) = check_access(&req, &cfg, true) {
        return respond(&mut stream, status, "text/plain", why.as_bytes(), &[], false).await;
    }
    let replay = Replay { prefix: std::io::Cursor::new(head), inner: stream };
    let ws = tokio_tungstenite::accept_async(replay).await.map_err(std::io::Error::other)?;
    let (mut sink, mut source) = ws.split();
    // The session speaks line-framed JSON over a pipe; this task moves lines to and from frames.
    let (client, server) = tokio::io::duplex(1 << 20);
    let session = tokio::spawn(session(server));
    let (client_read, mut client_write) = tokio::io::split(client);
    let (lines_tx, mut lines_rx) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(client_read);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                    if !text.is_empty() && lines_tx.send(text).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    loop {
        tokio::select! {
            line = lines_rx.recv() => match line {
                Some(text) => {
                    if sink.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            frame = source.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if client_write.write_all(text.as_bytes()).await.is_err()
                        || client_write.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = keepalive.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    // EOF to the session ends it; its host then shuts down what the tab started.
    drop(client_write);
    let _ = session.await;
    Ok(())
}

/// Commands the desktop shell answers itself and a browser tab asks its server instead.
pub fn register_commands(dispatcher: &mut crate::rpc::Dispatcher) {
    use crate::catalogue::TrustClass;
    use ide_language::LanguageInfo;
    use serde_json::{Value, json};

    dispatcher.add("app_info", TrustClass::Always, |c| async move {
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "windowsBuildNumber": Value::Null,
            "configDir": c.host.config_dir,
            "dataDir": c.host.data_dir,
            "wayland": false,
            "windowButtons": ide_core::platform::WindowButtonLayout::default(),
        }))
    });
    dispatcher.add("languages", TrustClass::Always, |c| async move {
        Ok(c.host.languages.all().iter().map(LanguageInfo::from).collect::<Vec<_>>())
    });
    // No recent-project history yet: the welcome screen starts empty.
    dispatcher.add("recent_projects", TrustClass::Always, |_| async { Ok(Vec::<Value>::new()) });
    dispatcher.add("recent_project_groups", TrustClass::Always, |_| async { Ok(Vec::<Value>::new()) });
    dispatcher.add("set_recent_project_groups", TrustClass::Always, |_| async { Ok(Vec::<Value>::new()) });
    dispatcher.add("remove_recent_project", TrustClass::Always, |_| async { Ok(Value::Null) });
    // Fonts are the browser's: it lists its own.
    dispatcher.add("system_font_families", TrustClass::Always, |_| async { Ok(Vec::<String>::new()) });
    dispatcher.add("log_frontend", TrustClass::Always, |c| async move {
        let level: String = c.arg("level")?;
        let message: String = c.arg("message")?;
        match level.as_str() {
            "error" => tracing::error!(target: "frontend", "{message}"),
            "warn" => tracing::warn!(target: "frontend", "{message}"),
            "debug" => tracing::debug!(target: "frontend", "{message}"),
            _ => tracing::info!(target: "frontend", "{message}"),
        }
        Ok(Value::Null)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_leave_the_web_root() {
        assert_eq!(safe_relative("/"), Some(PathBuf::from("index.html")));
        assert_eq!(safe_relative("/assets/app.js"), Some(PathBuf::from("assets/app.js")));
        assert_eq!(safe_relative("/docs/"), Some(PathBuf::from("docs/index.html")));
        assert_eq!(safe_relative("/../etc/passwd"), None);
        assert_eq!(safe_relative("/%2e%2e/etc/passwd"), None);
        assert_eq!(safe_relative("/a\\b"), None);
    }

    fn request(headers: &[(&str, &str)], method: &str, query: &str) -> Request {
        Request {
            method: method.into(),
            path: "/".into(),
            query: query.into(),
            headers: headers.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
        }
    }

    fn config(token: Option<&str>) -> WebConfig {
        WebConfig { web_root: PathBuf::new(), token: token.map(Into::into) }
    }

    #[test]
    fn a_token_is_required_and_becomes_a_cookie() {
        let cfg = config(Some("s3cret"));
        assert!(matches!(check_access(&request(&[], "GET", ""), &cfg, false), Access::Denied(401, _)));
        assert!(matches!(check_access(&request(&[], "GET", "token=wrong"), &cfg, false), Access::Denied(401, _)));
        assert!(matches!(
            check_access(&request(&[], "GET", "token=s3cret"), &cfg, false),
            Access::Granted { set_cookie: true }
        ));
        let with_cookie = request(&[("Cookie", "a=b; openidea_token=s3cret")], "GET", "");
        assert!(matches!(check_access(&with_cookie, &cfg, false), Access::Granted { set_cookie: false }));
        // The socket never takes the token from the address: only the cookie of the page.
        assert!(matches!(check_access(&request(&[], "GET", "token=s3cret"), &cfg, true), Access::Denied(401, _)));
    }

    #[test]
    fn a_socket_must_come_from_the_page_origin() {
        let cfg = config(None);
        let same = request(&[("Host", "ide.local:8080"), ("Origin", "http://ide.local:8080")], "GET", "");
        assert!(matches!(check_access(&same, &cfg, true), Access::Granted { .. }));
        let other = request(&[("Host", "ide.local:8080"), ("Origin", "http://ide.local:3000")], "GET", "");
        assert!(matches!(check_access(&other, &cfg, true), Access::Denied(403, _)));
        let proxied = request(
            &[
                ("Host", "10.0.0.7:8080"),
                ("X-Forwarded-Host", "lab.example.com"),
                ("Origin", "https://lab.example.com"),
            ],
            "GET",
            "",
        );
        assert!(matches!(check_access(&proxied, &cfg, true), Access::Granted { .. }));
    }

    #[test]
    fn index_is_marked_as_served_by_the_server() {
        let out = String::from_utf8(mark_as_web(b"<html><head><title>x</title></head></html>")).unwrap();
        assert!(out.starts_with(r#"<html><head><meta name="openidea-web""#), "{out}");
    }
}
