//! The web server every instance runs (read-only).
//!
//! * `/` and `/f/<share>/…/` — plain HTML pages, so phones and other computers
//!   without sls can browse and download with a web browser
//! * `/api/info`, `/api/list/<share>/…` — JSON for other instances
//! * `/f/<share>/<path…>` — file contents, with `Range` support

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use crate::http::{self, Range, Request, percent_encode};
use crate::protocol::{Info, PROTOCOL_VERSION};
use crate::shares::{self, Denied, Share};

const MAX_CONNECTIONS: usize = 64;

/// What this instance shares; changes are visible to the next request.
pub struct State {
    pub id: String,
    name: RwLock<String>,
    shares: RwLock<Vec<Share>>,
}

impl State {
    pub fn new(id: &str, name: &str, shares: Vec<Share>) -> Arc<State> {
        Arc::new(State {
            id: id.to_string(),
            name: RwLock::new(name.to_string()),
            shares: RwLock::new(shares),
        })
    }

    pub fn name(&self) -> String {
        self.name.read().unwrap().clone()
    }

    pub fn set_name(&self, name: &str) {
        *self.name.write().unwrap() = name.to_string();
    }

    pub fn shares(&self) -> Vec<Share> {
        self.shares.read().unwrap().clone()
    }

    pub fn set_shares(&self, shares: Vec<Share>) {
        *self.shares.write().unwrap() = shares;
    }

    pub fn info(&self) -> Info {
        Info {
            id: self.id.clone(),
            name: self.name(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            shares: self.shares().into_iter().map(|s| s.name).collect(),
        }
    }

    fn share_root(&self, name: &str) -> Option<PathBuf> {
        self.shares
            .read()
            .unwrap()
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.path.clone())
    }
}

pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Listens on all interfaces; `port` 0 picks a free one.
    pub fn start(state: Arc<State>, port: u16) -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let active = Arc::new(AtomicUsize::new(0));
        thread::Builder::new()
            .name("http-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let mut s = stream;
                        let _ = text_reply(&mut s, 503, "Too many connections");
                        continue;
                    }
                    let state = state.clone();
                    let active2 = active.clone();
                    let spawned =
                        thread::Builder::new()
                            .name("http-conn".into())
                            .spawn(move || {
                                let _ = serve_connection(&state, stream);
                                active2.fetch_sub(1, Ordering::SeqCst);
                            });
                    if spawned.is_err() {
                        // The closure (and its counter decrement) was dropped unrun.
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })?;
        Ok(Server { port, stop })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Binds `port`, or any free port if it is taken.
    pub fn start_with_fallback(state: Arc<State>, port: u16) -> io::Result<Server> {
        Server::start(state.clone(), port).or_else(|_| Server::start(state, 0))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake up the accept loop so it notices.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_millis(200),
        );
    }
}

enum Reply {
    Text(u16, &'static str, Vec<u8>),
    Redirect(String),
    File(File, u64, Option<&'static str>),
}

fn text_reply(w: &mut impl Write, status: u16, msg: &str) -> io::Result<()> {
    let body = format!("{msg}\n");
    http::write_head(
        w,
        status,
        &[
            ("Content-Type", "text/plain; charset=utf-8".into()),
            ("Content-Length", body.len().to_string()),
        ],
    )?;
    w.write_all(body.as_bytes())
}

fn serve_connection(state: &State, mut stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let req = match http::read_request(&mut BufReader::new(stream.try_clone()?)) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return text_reply(&mut stream, 400, "Bad request");
        }
        Err(e) => return Err(e),
    };
    let head_only = req.method == "HEAD";
    if req.method != "GET" && !head_only {
        return text_reply(&mut stream, 405, "Only GET is supported");
    }
    match route(state, &req) {
        Reply::Text(status, ctype, body) => {
            http::write_head(
                &mut stream,
                status,
                &[
                    ("Content-Type", ctype.into()),
                    ("Content-Length", body.len().to_string()),
                ],
            )?;
            if !head_only {
                stream.write_all(&body)?;
            }
            Ok(())
        }
        Reply::Redirect(to) => http::write_head(
            &mut stream,
            301,
            &[("Location", to), ("Content-Length", "0".into())],
        ),
        Reply::File(file, len, ctype) => send_file(&mut stream, &req, file, len, ctype, head_only),
    }
}

fn send_file(
    stream: &mut TcpStream,
    req: &Request,
    mut file: File,
    len: u64,
    ctype: Option<&'static str>,
    head_only: bool,
) -> io::Result<()> {
    use std::io::Seek;
    let mut headers = vec![
        (
            "Content-Type",
            ctype.unwrap_or("application/octet-stream").to_string(),
        ),
        ("Accept-Ranges", "bytes".into()),
    ];
    let (status, start, count) = match http::parse_range(req.header("range"), len) {
        Range::Full => (200, 0, len),
        Range::Part(a, b) => {
            headers.push(("Content-Range", format!("bytes {a}-{b}/{len}")));
            (206, a, b - a + 1)
        }
        Range::Unsatisfiable => {
            headers.push(("Content-Range", format!("bytes */{len}")));
            headers.push(("Content-Length", "0".into()));
            return http::write_head(stream, 416, &headers);
        }
    };
    headers.push(("Content-Length", count.to_string()));
    http::write_head(stream, status, &headers)?;
    if head_only {
        return Ok(());
    }
    file.seek(io::SeekFrom::Start(start))?;
    io::copy(&mut file.take(count), stream)?;
    Ok(())
}

fn denied(d: Denied) -> Reply {
    match d {
        Denied::NotFound => not_found(),
        Denied::Forbidden => Reply::Text(403, "text/plain; charset=utf-8", b"Forbidden\n".to_vec()),
    }
}

fn not_found() -> Reply {
    Reply::Text(404, "text/plain; charset=utf-8", b"Not found\n".to_vec())
}

fn json(value: &impl serde::Serialize) -> Reply {
    Reply::Text(
        200,
        "application/json",
        serde_json::to_vec(value).unwrap_or_default(),
    )
}

fn route(state: &State, req: &Request) -> Reply {
    let trailing_slash = req.path.ends_with('/');
    let mut segments = Vec::new();
    for raw in req.path.split('/').filter(|s| !s.is_empty()) {
        match http::percent_decode(raw) {
            Some(s) => segments.push(s),
            None => return Reply::Text(400, "text/plain; charset=utf-8", b"Bad path\n".to_vec()),
        }
    }
    let seg: Vec<&str> = segments.iter().map(String::as_str).collect();
    match seg.as_slice() {
        [] => Reply::Text(
            200,
            "text/html; charset=utf-8",
            index_page(state).into_bytes(),
        ),
        ["api", "info"] => json(&state.info()),
        ["api", "list", share, rest @ ..] => {
            let Some(root) = state.share_root(share) else {
                return not_found();
            };
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            match shares::resolve(&root, &rest) {
                Ok(dir) if dir.is_dir() => match shares::list_dir(&root, &dir) {
                    Ok(list) => json(&list),
                    Err(_) => denied(Denied::Forbidden),
                },
                Ok(_) => not_found(),
                Err(d) => denied(d),
            }
        }
        ["f", share, rest @ ..] => {
            let Some(root) = state.share_root(share) else {
                return not_found();
            };
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            let path = match shares::resolve(&root, &rest) {
                Ok(p) => p,
                Err(d) => return denied(d),
            };
            if path.is_dir() {
                if !trailing_slash {
                    return Reply::Redirect(format!("{}/", req.path));
                }
                match shares::list_dir(&root, &path) {
                    Ok(list) => Reply::Text(
                        200,
                        "text/html; charset=utf-8",
                        dir_page(state, share, &rest, &list).into_bytes(),
                    ),
                    Err(_) => denied(Denied::Forbidden),
                }
            } else {
                match File::open(&path).and_then(|f| f.metadata().map(|m| (f, m.len()))) {
                    Ok((f, len)) => Reply::File(f, len, content_type(&path)),
                    Err(_) => denied(Denied::Forbidden),
                }
            }
        }
        _ => not_found(),
    }
}

fn content_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "txt" | "md" | "log" | "csv" | "srt" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "zip" => "application/zip",
        _ => return None,
    })
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const STYLE: &str = "<meta name=viewport content='width=device-width,initial-scale=1'>\
<style>body{font-family:sans-serif;max-width:50em;margin:1em auto;padding:0 1em;line-height:1.6}\
a{text-decoration:none}td{padding:.1em .6em .1em 0}td.s{text-align:right;color:#777;white-space:nowrap}\
@media(prefers-color-scheme:dark){body{background:#1e1e1e;color:#ddd}a{color:#8ab4f8}}</style>";

fn index_page(state: &State) -> String {
    let name = html_escape(&state.name());
    let mut out =
        format!("<!doctype html><meta charset=utf-8><title>{name}</title>{STYLE}<h1>{name}</h1>");
    let shares = state.shares();
    if shares.is_empty() {
        out.push_str("<p>Nothing is shared.</p>");
    }
    out.push_str("<ul>");
    for s in shares {
        out.push_str(&format!(
            "<li><a href=\"/f/{}/\">📁 {}</a></li>",
            percent_encode(&s.name),
            html_escape(&s.name)
        ));
    }
    out.push_str("</ul>");
    out
}

fn dir_page(
    state: &State,
    share: &str,
    rest: &[String],
    list: &[crate::protocol::Entry],
) -> String {
    let title = html_escape(&format!("{} / {}", state.name(), share));
    let mut out = format!("<!doctype html><meta charset=utf-8><title>{title}</title>{STYLE}");
    // Breadcrumbs.
    let mut href = String::from("/");
    out.push_str(&format!(
        "<h2><a href=\"/\">{}</a>",
        html_escape(&state.name())
    ));
    for s in std::iter::once(share).chain(rest.iter().map(String::as_str)) {
        href = if href == "/" {
            format!("/f/{}/", percent_encode(s))
        } else {
            format!("{href}{}/", percent_encode(s))
        };
        out.push_str(&format!(" / <a href=\"{href}\">{}</a>", html_escape(s)));
    }
    out.push_str("</h2><table>");
    if list.is_empty() {
        out.push_str("<tr><td>This folder is empty.</td></tr>");
    }
    for e in list {
        let link = percent_encode(&e.name);
        if e.dir {
            out.push_str(&format!(
                "<tr><td><a href=\"{link}/\">📁 {}/</a></td><td></td></tr>",
                html_escape(&e.name)
            ));
        } else {
            out.push_str(&format!(
                "<tr><td><a href=\"{link}\">📄 {}</a></td><td class=s>{}</td></tr>",
                html_escape(&e.name),
                crate::human_size(e.size)
            ));
        }
    }
    out.push_str("</table>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Arc<State>, Server) {
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("My Music");
        fs::create_dir_all(music.join("Album")).unwrap();
        fs::write(music.join("Album/01 — song.mp3"), b"0123456789").unwrap();
        fs::write(tmp.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), music.join("link")).unwrap();
        let state = State::new("abc", "Test <PC>", shares::from_paths(&[music]));
        let server = Server::start(state.clone(), 0).unwrap();
        (tmp, state, server)
    }

    fn get(
        server: &Server,
        path: &str,
        headers: &[(&str, String)],
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        let resp = http::get(addr, path, headers, Duration::from_secs(5)).unwrap();
        let (status, h) = (resp.status, resp.headers.clone());
        (status, h, resp.read_all(1 << 20).unwrap())
    }

    #[test]
    fn info_and_listing() {
        let (_tmp, state, server) = setup();
        let (status, _, body) = get(&server, "/api/info", &[]);
        assert_eq!(status, 200);
        let info: Info = serde_json::from_slice(&body).unwrap();
        assert_eq!(info.id, "abc");
        assert_eq!(info.shares, ["My Music"]);

        let (status, _, body) = get(&server, "/api/list/My%20Music", &[]);
        assert_eq!(status, 200);
        let list: Vec<crate::protocol::Entry> = serde_json::from_slice(&body).unwrap();
        // The link leading outside is not listed.
        assert_eq!(list.len(), 1);
        assert!(list[0].dir && list[0].name == "Album");

        // Shares can change while running.
        state.set_shares(vec![]);
        assert_eq!(get(&server, "/api/list/My%20Music", &[]).0, 404);
    }

    #[test]
    fn files_and_ranges() {
        let (_tmp, _state, server) = setup();
        let path = http::build_path("/f", &["My Music", "Album", "01 — song.mp3"]);
        let (status, headers, body) = get(&server, &path, &[]);
        assert_eq!(status, 200);
        assert_eq!(body, b"0123456789");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "audio/mpeg")
        );

        let (status, headers, body) = get(&server, &path, &[("Range", "bytes=4-".into())]);
        assert_eq!(status, 206);
        assert_eq!(body, b"456789");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Content-Range" && v == "bytes 4-9/10")
        );
        assert_eq!(get(&server, &path, &[("Range", "bytes=10-".into())]).0, 416);
    }

    #[test]
    fn nothing_outside_the_share() {
        let (_tmp, _state, server) = setup();
        for path in [
            "/f/My%20Music/..%2Fsecret.txt",
            "/f/My%20Music/../secret.txt",
            "/f/My%20Music/%2E%2E/secret.txt",
            "/f/My%20Music/link",
            "/api/list/My%20Music/..",
        ] {
            let (status, _, body) = get(&server, path, &[]);
            assert!(status == 403 || status == 404, "{path}: {status}");
            assert!(!body.starts_with(b"secret"), "{path}");
        }
        assert_eq!(get(&server, "/f/Other/x", &[]).0, 404);
        assert_eq!(get(&server, "/nope", &[]).0, 404);
        assert_eq!(get(&server, "/f/%zz", &[]).0, 400);
    }

    #[test]
    fn browser_pages() {
        let (_tmp, _state, server) = setup();
        let (status, _, body) = get(&server, "/", &[]);
        assert_eq!(status, 200);
        let page = String::from_utf8(body).unwrap();
        assert!(page.contains("Test &lt;PC&gt;"));
        assert!(page.contains("href=\"/f/My%20Music/\""));

        let (status, headers, _) = get(&server, "/f/My%20Music", &[]);
        assert_eq!(status, 301);
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Location" && v == "/f/My%20Music/")
        );

        let (status, _, body) = get(&server, "/f/My%20Music/Album/", &[]);
        assert_eq!(status, 200);
        let page = String::from_utf8(body).unwrap();
        assert!(page.contains("href=\"01%20%E2%80%94%20song.mp3\""));
        assert!(page.contains("10 bytes"));
    }

    #[test]
    fn other_methods_and_garbage() {
        let (_tmp, _state, server) = setup();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        for (raw, expect) in [
            (&b"POST / HTTP/1.1\r\n\r\n"[..], "405"),
            (&b"nonsense\r\n\r\n"[..], "400"),
        ] {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(raw).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            assert!(out.starts_with(&format!("HTTP/1.1 {expect}")), "{out}");
        }
    }
}
