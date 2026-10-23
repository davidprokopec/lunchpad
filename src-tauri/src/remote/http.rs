//! The little HTTP the remote needs: one request per connection, `GET` only,
//! the phone page and its files, and the WebSocket upgrade on `/ws`.
//!
//! Requests are checked against DNS rebinding: the `Host` must be an IP
//! address, `localhost` or a `.local` name (a hostile site's own domain is
//! refused), and a WebSocket opened by a browser must come from a page of this
//! same host (`Origin` equal to `http://<Host>`).

use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt};

/// The request line and headers may not exceed this.
pub const MAX_HEAD: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn header_has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .any(|(_, v)| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }

    /// A well-formed WebSocket upgrade (RFC 6455 §4.2.1); returns the client key.
    pub fn websocket_key(&self) -> Option<&str> {
        let upgrade = self.header_has_token("upgrade", "websocket")
            && self.header_has_token("connection", "upgrade");
        let version = self.header("sec-websocket-version").map(str::trim) == Some("13");
        let key = self
            .header("sec-websocket-key")
            .map(str::trim)
            .filter(|k| !k.is_empty())?;
        (upgrade && version).then_some(key)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HeadError {
    TooLarge,
    Malformed,
    Closed,
    /// Bytes after the head: nothing may follow a GET before our answer.
    TrailingBytes,
}

/// Read one request head. Reads byte by byte so nothing past the head is consumed:
/// after a WebSocket upgrade the stream belongs to the WebSocket.
pub async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Request, HeadError> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(HeadError::Closed);
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() >= MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
    }
    parse_head(&buf)
}

pub fn parse_head(buf: &[u8]) -> Result<Request, HeadError> {
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf) {
        Ok(httparse::Status::Complete(len)) if len == buf.len() => {}
        Ok(httparse::Status::Complete(_)) => return Err(HeadError::TrailingBytes),
        Ok(httparse::Status::Partial) => return Err(HeadError::Malformed),
        Err(httparse::Error::TooManyHeaders) => return Err(HeadError::TooLarge),
        Err(_) => return Err(HeadError::Malformed),
    }
    let method = req.method.ok_or(HeadError::Malformed)?.to_string();
    let path = req.path.ok_or(HeadError::Malformed)?.to_string();
    let headers = req
        .headers
        .iter()
        .map(|h| {
            std::str::from_utf8(h.value)
                .map(|v| (h.name.to_ascii_lowercase(), v.trim().to_string()))
                .map_err(|_| HeadError::Malformed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Request {
        method,
        path,
        headers,
    })
}

/// Whether a `Host` header names this machine in a way a foreign site cannot:
/// an IP literal, `localhost` or an mDNS `.local` name, with our port or none.
pub fn host_allowed(host: &str, port: u16) -> bool {
    let host = host.trim();
    let (name, host_port) = if let Some(rest) = host.strip_prefix('[') {
        // [v6]:port
        let Some((addr, after)) = rest.split_once(']') else {
            return false;
        };
        if addr.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match after {
            "" => return true,
            p => (addr, p.strip_prefix(':')),
        }
    } else {
        match host.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (host, None),
        }
    };
    if let Some(p) = host_port {
        if p.parse::<u16>().ok() != Some(port) {
            return false;
        }
    }
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    name.parse::<IpAddr>().is_ok()
        || name == "localhost"
        || (name.ends_with(".local")
            && name.len() > ".local".len()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'))
}

/// A browser always sends `Origin` on a WebSocket; it must be this very host.
/// Clients without one (not a browser, so not driven by a web page) pass.
pub fn origin_allowed(origin: Option<&str>, host: &str) -> bool {
    match origin {
        None => true,
        Some(origin) => origin
            .trim()
            .eq_ignore_ascii_case(&format!("http://{}", host.trim())),
    }
}

/// Which file a path serves: the phone page, a built asset, or a root icon.
/// Anything else (the desktop app's `index.html`, `..`, queries) is not served.
pub fn asset_for(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path == "/" || path == "/remote.html" {
        return Some("remote.html".into());
    }
    let safe = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if let Some(name) = path.strip_prefix("/assets/") {
        return safe(name).then(|| format!("assets/{name}"));
    }
    let name = path.strip_prefix('/')?;
    let icon = [".png", ".svg", ".ico", ".webmanifest"]
        .iter()
        .any(|ext| name.ends_with(ext));
    (safe(name) && icon).then(|| name.to_string())
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: String,
    pub body: Vec<u8>,
    pub extra: Vec<(&'static str, String)>,
}

impl Response {
    pub fn text(status: u16, reason: &'static str, body: &str) -> Response {
        Response {
            status,
            reason,
            content_type: "text/plain; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            extra: Vec::new(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Where the phone page's files come from: the app's bundled frontend.
pub trait AssetSource: Send + Sync {
    /// Bytes and MIME type of a file of the built frontend (`remote.html`, `assets/…`).
    fn get(&self, path: &str) -> Option<(Vec<u8>, String)>;
}

/// Shown when the build has no `remote.html` yet.
pub const PLACEHOLDER_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Lunchpad remote</title></head><body style=\"background:#0b0b0f;color:#eee;font-family:system-ui;padding:2rem\"><h1>Lunchpad remote</h1><p>The remote page is not part of this build.</p></body></html>";

/// The answer to a plain GET.
pub fn static_response(req: &Request, assets: &dyn AssetSource) -> Response {
    let Some(file) = asset_for(&req.path) else {
        return Response::text(404, "Not Found", "not found");
    };
    let html = file == "remote.html";
    let (body, content_type) = match assets.get(&file) {
        Some(found) => found,
        None if html => (PLACEHOLDER_PAGE.as_bytes().to_vec(), "text/html".into()),
        None => return Response::text(404, "Not Found", "not found"),
    };
    let mut extra = vec![(
        "Cache-Control",
        if html {
            "no-store".to_string()
        } else {
            "public, max-age=31536000, immutable".to_string()
        },
    )];
    if html {
        let host = req.header("host").unwrap_or_default();
        extra.push((
            "Content-Security-Policy",
            format!("default-src 'self'; connect-src 'self' ws://{host}; img-src 'self' data:; style-src 'self' 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
        ));
    }
    let content_type = if content_type.starts_with("text/") && !content_type.contains("charset") {
        format!("{content_type}; charset=utf-8")
    } else {
        content_type
    };
    Response {
        status: 200,
        reason: "OK",
        content_type,
        body,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn hosts_that_a_foreign_site_could_rebind_are_refused() {
        for ok in [
            "192.168.1.5:7373",
            "192.168.1.5",
            "127.0.0.1:7373",
            "localhost:7373",
            "Davids-Mac.local:7373",
            "davids-mac.local.",
            "[::1]:7373",
            "[fe80::1]",
        ] {
            assert!(host_allowed(ok, 7373), "{ok}");
        }
        for bad in [
            "evil.example:7373",
            "evil.example",
            "192.168.1.5:80",
            "localhost:1",
            "",
            ".local",
            "a.local:x",
            "[zz]:7373",
            "1.2.3.4.nip.io:7373",
            "lunchpad.local@evil.example",
        ] {
            assert!(!host_allowed(bad, 7373), "{bad}");
        }
    }

    #[test]
    fn a_browser_websocket_must_come_from_this_host() {
        assert!(origin_allowed(None, "192.168.1.5:7373"), "not a browser");
        assert!(origin_allowed(
            Some("http://192.168.1.5:7373"),
            "192.168.1.5:7373"
        ));
        assert!(!origin_allowed(
            Some("http://evil.example"),
            "192.168.1.5:7373"
        ));
        assert!(!origin_allowed(Some("null"), "192.168.1.5:7373"));
        assert!(!origin_allowed(
            Some("https://192.168.1.5:7373"),
            "192.168.1.5:7373"
        ));
    }

    #[test]
    fn only_the_phone_page_and_its_files_are_served() {
        assert_eq!(asset_for("/").as_deref(), Some("remote.html"));
        assert_eq!(asset_for("/?pair=1").as_deref(), Some("remote.html"));
        assert_eq!(
            asset_for("/assets/remote-abc123.js").as_deref(),
            Some("assets/remote-abc123.js")
        );
        assert_eq!(asset_for("/icon.png").as_deref(), Some("icon.png"));
        for bad in [
            "/index.html",
            "/assets/../index.html",
            "/assets/",
            "/assets/a/b.js",
            "/../secret",
            "/.env",
            "/settings.json",
            "/ws",
        ] {
            assert_eq!(asset_for(bad), None, "{bad}");
        }
    }

    #[test]
    fn heads_parse_and_upgrades_are_recognised() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: 192.168.1.5:7373\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let r = parse_head(raw).unwrap();
        assert_eq!(r.path, "/ws");
        assert_eq!(r.header("host"), Some("192.168.1.5:7373"));
        assert_eq!(r.websocket_key(), Some("dGhlIHNhbXBsZSBub25jZQ=="));
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap()
                .websocket_key(),
            None
        );
        assert_eq!(parse_head(b"GARBAGE\r\n\r\n"), Err(HeadError::Malformed));
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\r\n\r\nextra"),
            Err(HeadError::TrailingBytes)
        );
    }

    struct NoAssets;
    impl AssetSource for NoAssets {
        fn get(&self, _: &str) -> Option<(Vec<u8>, String)> {
            None
        }
    }

    #[test]
    fn a_build_without_the_page_serves_a_placeholder() {
        let r = static_response(&req("/", &[("host", "192.168.1.5:7373")]), &NoAssets);
        assert_eq!(r.status, 200);
        assert!(String::from_utf8(r.body.clone())
            .unwrap()
            .contains("Lunchpad remote"));
        let text = String::from_utf8(r.to_bytes()).unwrap();
        assert!(text.contains("Content-Security-Policy: default-src 'self'; connect-src 'self' ws://192.168.1.5:7373;"));
        assert!(text.contains("Cache-Control: no-store"));
        assert_eq!(
            static_response(&req("/assets/x.js", &[]), &NoAssets).status,
            404
        );
    }
}
