//! Talking to another instance: info, folder listings, downloads.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};

use crate::http;
use crate::protocol::{Entry, Info, Offer, OfferReply};
use crate::shares::is_safe_name;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Folders nested deeper than this are not followed (link loops).
const MAX_DEPTH: usize = 64;
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

fn json_get<T: serde::de::DeserializeOwned>(
    addr: SocketAddr,
    path: &str,
    timeout: Duration,
) -> anyhow::Result<T> {
    let resp = http::get(addr, path, &[], timeout)?;
    let status = resp.status;
    let body = resp.read_all(64 << 20)?;
    if status != 200 {
        bail!("{} ({status})", String::from_utf8_lossy(&body).trim());
    }
    Ok(serde_json::from_slice(&body)?)
}

pub fn fetch_info(addr: SocketAddr, timeout: Duration) -> anyhow::Result<Info> {
    json_get(addr, "/api/info", timeout)
}

fn segments<'a>(share: &'a str, path: &'a [String]) -> Vec<&'a str> {
    std::iter::once(share)
        .chain(path.iter().map(String::as_str))
        .collect()
}

fn json_post(addr: SocketAddr, path: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    let resp = http::post_json(addr, path, &body, Duration::from_secs(10))?;
    let status = resp.status;
    let body = resp.read_all(1 << 16).unwrap_or_default();
    match status {
        200 => Ok(()),
        // Older versions only know GET.
        405 => bail!("its sls is too old to receive files"),
        503 => bail!("it runs without a window and cannot be asked"),
        429 => bail!("too many offers are waiting for an answer there"),
        _ => bail!("{} ({status})", String::from_utf8_lossy(&body).trim()),
    }
}

/// Asks the instance at `addr` to download `offer` from us.
pub fn send_offer(addr: SocketAddr, offer: &Offer) -> anyhow::Result<()> {
    json_post(addr, "/api/offer", offer)
}

/// Tells the sender (at its address and `offer.port`) the answer.
pub fn send_reply(addr: SocketAddr, reply: &OfferReply) -> anyhow::Result<()> {
    json_post(addr, "/api/offer-reply", reply)
}

/// The contents of `path` (segments) inside `share`.
pub fn list(addr: SocketAddr, share: &str, path: &[String]) -> anyhow::Result<Vec<Entry>> {
    json_get(
        addr,
        &http::build_path("/api/list", &segments(share, path)),
        READ_TIMEOUT,
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
}

/// `dir/name`, or `dir/name (1).ext`, `dir/name (2).ext`, … if that exists.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

struct Job<'a> {
    addr: SocketAddr,
    share: &'a str,
    cancel: &'a AtomicBool,
    progress: Progress,
    on_progress: &'a mut dyn FnMut(&Progress),
    last_report: Instant,
}

impl Job<'_> {
    fn check_cancel(&self) -> anyhow::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(anyhow!(Cancelled))
        } else {
            Ok(())
        }
    }

    fn report(&mut self, force: bool) {
        if force || self.last_report.elapsed() >= PROGRESS_EVERY {
            self.last_report = Instant::now();
            (self.on_progress)(&self.progress);
        }
    }

    /// Every file below `path`: (remote segments, local relative path, size).
    fn collect(
        &self,
        path: &[String],
        rel: &Path,
        depth: usize,
        files: &mut Vec<(Vec<String>, PathBuf, u64)>,
        dirs: &mut Vec<PathBuf>,
    ) -> anyhow::Result<()> {
        self.check_cancel()?;
        if depth > MAX_DEPTH {
            return Ok(());
        }
        for e in list(self.addr, self.share, path)? {
            if !is_safe_name(&e.name) {
                continue;
            }
            let mut sub = path.to_vec();
            sub.push(e.name.clone());
            let rel = rel.join(&e.name);
            if e.dir {
                dirs.push(rel.clone());
                self.collect(&sub, &rel, depth + 1, files, dirs)?;
            } else {
                files.push((sub, rel, e.size));
            }
        }
        Ok(())
    }

    /// Downloads into `dest` via `dest.sls-part`.
    fn fetch_file(&mut self, path: &[String], dest: &Path) -> anyhow::Result<()> {
        self.check_cancel()?;
        let url = http::build_path("/f", &segments(self.share, path));
        let mut resp = http::get(self.addr, &url, &[], READ_TIMEOUT)?;
        if resp.status != 200 {
            let status = resp.status;
            let body = resp.read_all(1 << 16).unwrap_or_default();
            bail!(
                "{}: {} ({status})",
                path.join("/"),
                String::from_utf8_lossy(&body).trim()
            );
        }
        let len = resp.len;
        let mut part = dest.as_os_str().to_owned();
        part.push(".sls-part");
        let part = PathBuf::from(part);
        let result = (|| -> anyhow::Result<()> {
            let mut out = File::create(&part).with_context(|| part.display().to_string())?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut got = 0u64;
            loop {
                self.check_cancel()?;
                let n = resp.body.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                got += n as u64;
                self.progress.done += n as u64;
                self.report(false);
            }
            if got != len {
                bail!("connection lost while downloading {}", path.join("/"));
            }
            out.sync_all()?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(fs::rename(&part, dest)?),
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e)
            }
        }
    }
}

/// Returned (inside the error) when a download was cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

/// Downloads a file or a whole folder (`path` empty = the whole share) into
/// `dest_dir`, never overwriting anything: an existing name gets " (1)".
/// Returns the new file or folder.  A cancelled folder download is removed.
pub fn download(
    addr: SocketAddr,
    share: &str,
    path: &[String],
    is_dir: bool,
    dest_dir: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> anyhow::Result<PathBuf> {
    let name = path.last().map(String::as_str).unwrap_or(share);
    if !is_safe_name(name) {
        bail!("unusable name {name:?}");
    }
    fs::create_dir_all(dest_dir).with_context(|| dest_dir.display().to_string())?;
    let mut job = Job {
        addr,
        share,
        cancel,
        progress: Progress::default(),
        on_progress,
        last_report: Instant::now(),
    };
    if !is_dir {
        let size = list(addr, share, &path[..path.len().saturating_sub(1)])
            .ok()
            .and_then(|l| l.into_iter().find(|e| e.name == name))
            .map_or(0, |e| e.size);
        job.progress.total = size;
        job.progress.files_total = 1;
        job.report(true);
        let dest = unique_path(dest_dir, name);
        job.fetch_file(path, &dest)?;
        job.progress.files_done = 1;
        job.report(true);
        return Ok(dest);
    }

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    job.collect(path, Path::new(""), 0, &mut files, &mut dirs)?;
    job.progress.total = files.iter().map(|f| f.2).sum();
    job.progress.files_total = files.len();
    job.report(true);

    let dest = unique_path(dest_dir, name);
    fs::create_dir(&dest).with_context(|| dest.display().to_string())?;
    let result = (|| -> anyhow::Result<()> {
        for d in &dirs {
            fs::create_dir_all(dest.join(d))?;
        }
        for (remote, rel, _) in &files {
            job.fetch_file(remote, &dest.join(rel))?;
            job.progress.files_done += 1;
            job.report(false);
        }
        job.report(true);
        Ok(())
    })();
    match result {
        Ok(()) => Ok(dest),
        Err(e) => {
            if is_cancelled(&e) {
                let _ = fs::remove_dir_all(&dest);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Server, State};
    use crate::shares;
    use std::net::Ipv4Addr;

    fn serve(dir: &Path) -> (Server, SocketAddr) {
        let state = State::new("x", "X", shares::from_paths(&[dir.to_path_buf()]));
        let server = Server::start(state, 0).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        (server, addr)
    }

    #[test]
    fn unique_names() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert_eq!(unique_path(d, "a.txt"), d.join("a.txt"));
        fs::write(d.join("a.txt"), "").unwrap();
        fs::write(d.join("a (1).txt"), "").unwrap();
        assert_eq!(unique_path(d, "a.txt"), d.join("a (2).txt"));
        fs::create_dir(d.join("Music")).unwrap();
        assert_eq!(unique_path(d, "Music"), d.join("Music (1)"));
        fs::write(d.join(".bashrc"), "").unwrap();
        assert_eq!(unique_path(d, ".bashrc"), d.join(".bashrc (1)"));
    }

    #[test]
    fn downloads_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Photos");
        fs::create_dir_all(src.join("2024/summer")).unwrap();
        fs::create_dir_all(src.join("empty")).unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("2024/summer/big.bin"), &big).unwrap();
        fs::write(src.join("a b.txt"), "hello").unwrap();
        let (_server, addr) = serve(&src);
        let dl = tmp.path().join("dl");
        let cancel = AtomicBool::new(false);

        let info = fetch_info(addr, Duration::from_secs(2)).unwrap();
        assert_eq!(info.shares, ["Photos"]);

        let mut last = Progress::default();
        let file = download(
            addr,
            "Photos",
            &["a b.txt".into()],
            false,
            &dl,
            &cancel,
            &mut |p| last = p.clone(),
        )
        .unwrap();
        assert_eq!(file, dl.join("a b.txt"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
        assert_eq!((last.done, last.total, last.files_done), (5, 5, 1));

        // Again: does not overwrite.
        let file = download(
            addr,
            "Photos",
            &["a b.txt".into()],
            false,
            &dl,
            &cancel,
            &mut |_| (),
        )
        .unwrap();
        assert_eq!(file, dl.join("a b (1).txt"));

        // The whole share.
        let folder = download(addr, "Photos", &[], true, &dl, &cancel, &mut |p| {
            last = p.clone()
        })
        .unwrap();
        assert_eq!(folder, dl.join("Photos"));
        assert_eq!(fs::read(folder.join("2024/summer/big.bin")).unwrap(), big);
        assert!(folder.join("empty").is_dir());
        assert_eq!(last.done, last.total);
        assert_eq!((last.files_done, last.files_total), (2, 2));
        assert!(!fs::read_dir(&folder).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".sls-part")
        }));

        // A subfolder.
        let sub = download(
            addr,
            "Photos",
            &["2024".into()],
            true,
            &dl,
            &cancel,
            &mut |_| (),
        )
        .unwrap();
        assert!(sub.join("summer/big.bin").is_file());
    }

    #[test]
    fn cancel_and_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("S");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), vec![0u8; 2_000_000]).unwrap();
        let (_server, addr) = serve(&src);
        let dl = tmp.path().join("dl");

        let cancel = AtomicBool::new(true);
        let err = download(addr, "S", &[], true, &dl, &cancel, &mut |_| ()).unwrap_err();
        assert!(is_cancelled(&err));
        let cancel = AtomicBool::new(false);
        let err = download(addr, "S", &[], true, &dl, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed)
        })
        .unwrap_err();
        assert!(is_cancelled(&err));
        assert_eq!(
            fs::read_dir(&dl).unwrap().count(),
            0,
            "cancelled download leaves nothing"
        );

        let cancel = AtomicBool::new(false);
        let err = download(
            addr,
            "S",
            &["missing".into()],
            false,
            &dl,
            &cancel,
            &mut |_| (),
        )
        .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
        assert!(download(addr, "S", &["..".into()], false, &dl, &cancel, &mut |_| ()).is_err());
        assert!(list(addr, "Nope", &[]).is_err());
    }
}
