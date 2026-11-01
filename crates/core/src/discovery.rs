//! Finding the other instances.
//!
//! Every instance advertises `_sls._tcp` over mDNS (TXT: `id`, `name`) and
//! browses for the others.  mDNS only tells *where* an instance is: what it
//! shares comes from polling its `/api/info` every few seconds, which also
//! notices instances that vanished without saying goodbye.  Addresses from
//! the `peers` setting are polled the same way (networks without multicast).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::client;
use crate::protocol::Info;

pub const SERVICE_TYPE: &str = "_sls._tcp.local.";
const POLL_EVERY: Duration = Duration::from_secs(3);
const INFO_TIMEOUT: Duration = Duration::from_secs(3);
/// Failed polls before a peer is hidden.
const MISSES_TO_HIDE: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    /// The address that answered.
    pub addr: SocketAddr,
    pub version: String,
    pub shares: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub id: String,
    pub name: String,
    /// Advertise this web server port over mDNS (None: only look).
    pub advertise_port: Option<u16>,
    pub mdns: bool,
    /// "host:port" addresses to poll directly.
    pub static_peers: Vec<String>,
}

/// Order in which advertised addresses are tried: typical home LAN ranges
/// first; container bridges (Docker uses 172.17-31.x) and loopback last.
pub fn address_rank(addr: &SocketAddr) -> u8 {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => 4,
        IpAddr::V4(ip) if ip.octets()[0] == 192 && ip.octets()[1] == 168 => 0,
        IpAddr::V4(ip) if ip.octets()[0] == 10 => 1,
        IpAddr::V4(ip) if ip.is_private() => 2,
        _ => 3,
    }
}

enum Source {
    Mdns(Vec<SocketAddr>),
    Static(String),
}

impl Source {
    fn addresses(&self) -> Vec<SocketAddr> {
        let mut addrs = match self {
            Source::Mdns(a) => a.clone(),
            Source::Static(s) => s
                .to_socket_addrs()
                .map(|a| a.filter(|a| a.is_ipv4()).collect())
                .unwrap_or_default(),
        };
        addrs.sort_by_key(|a| (address_rank(a), *a));
        addrs.dedup();
        addrs
    }
}

struct Shared {
    my_id: String,
    /// Key: mDNS full name or "static:host:port".
    candidates: Mutex<HashMap<String, Source>>,
    stop: AtomicBool,
}

pub struct Discovery {
    shared: Arc<Shared>,
    wake: mpsc::Sender<()>,
    daemon: Option<mdns_sd::ServiceDaemon>,
    registered: Option<String>,
}

impl Discovery {
    /// Starts looking; `on_change` gets the full, sorted peer list whenever
    /// it changes (from a background thread).
    pub fn start(
        cfg: DiscoveryConfig,
        on_change: impl Fn(Vec<Peer>) + Send + 'static,
    ) -> anyhow::Result<Discovery> {
        let candidates = cfg
            .static_peers
            .iter()
            .map(|p| (format!("static:{p}"), Source::Static(p.clone())))
            .collect();
        let shared = Arc::new(Shared {
            my_id: cfg.id.clone(),
            candidates: Mutex::new(candidates),
            stop: AtomicBool::new(false),
        });
        let (wake, wake_rx) = mpsc::channel();
        let mut d = Discovery {
            shared: shared.clone(),
            wake: wake.clone(),
            daemon: None,
            registered: None,
        };
        if cfg.mdns {
            d.start_mdns(&cfg, wake)?;
        }
        let s = shared.clone();
        thread::Builder::new()
            .name("discovery-poll".into())
            .spawn(move || poll_loop(s, wake_rx, on_change))?;
        Ok(d)
    }

    /// Polls everyone right now (e.g. a refresh button).
    pub fn refresh(&self) {
        let _ = self.wake.send(());
    }

    fn start_mdns(&mut self, cfg: &DiscoveryConfig, wake: mpsc::Sender<()>) -> anyhow::Result<()> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        if let Some(port) = cfg.advertise_port {
            let props = [("id", cfg.id.as_str()), ("name", cfg.name.as_str())];
            let info = mdns_sd::ServiceInfo::new(
                SERVICE_TYPE,
                &cfg.id,
                &format!("sls-{}.local.", cfg.id),
                "",
                port,
                &props[..],
            )?
            .enable_addr_auto();
            self.registered = Some(info.get_fullname().to_string());
            daemon.register(info)?;
        }
        let events = daemon.browse(SERVICE_TYPE)?;
        let s = self.shared.clone();
        thread::Builder::new()
            .name("discovery-mdns".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    match ev {
                        mdns_sd::ServiceEvent::ServiceResolved(info) => {
                            if info.get_property_val_str("id") == Some(s.my_id.as_str()) {
                                continue;
                            }
                            let port = info.get_port();
                            let addrs: Vec<SocketAddr> = info
                                .get_addresses_v4()
                                .into_iter()
                                .map(|ip| SocketAddr::from((ip, port)))
                                .collect();
                            if addrs.is_empty() {
                                continue;
                            }
                            s.candidates
                                .lock()
                                .unwrap()
                                .insert(info.get_fullname().to_string(), Source::Mdns(addrs));
                            let _ = wake.send(());
                        }
                        mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                            s.candidates.lock().unwrap().remove(&fullname);
                            let _ = wake.send(());
                        }
                        _ => {}
                    }
                }
            })?;
        self.daemon = Some(daemon);
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.send(());
        if let Some(d) = self.daemon.take() {
            // Say goodbye so the others drop us at once.
            if let Some(name) = &self.registered
                && let Ok(rx) = d.unregister(name)
            {
                let _ = rx.recv_timeout(Duration::from_secs(1));
            }
            let _ = d.shutdown();
        }
    }
}

/// Asks the addresses in order; the first that answers wins.
fn ask(addrs: &[SocketAddr]) -> Option<(SocketAddr, Info)> {
    addrs
        .iter()
        .find_map(|a| client::fetch_info(*a, INFO_TIMEOUT).ok().map(|i| (*a, i)))
}

fn poll_loop(s: Arc<Shared>, wake: mpsc::Receiver<()>, on_change: impl Fn(Vec<Peer>)) {
    // Per candidate key: the last good answer and how many polls failed since.
    let mut known: HashMap<String, (Peer, u32)> = HashMap::new();
    let mut last: Option<Vec<Peer>> = None;
    while !s.stop.load(Ordering::Relaxed) {
        let targets: Vec<(String, Vec<SocketAddr>)> = s
            .candidates
            .lock()
            .unwrap()
            .iter()
            .map(|(k, src)| {
                let mut addrs = src.addresses();
                // The address that worked last time first.
                if let Some((p, _)) = known.get(k)
                    && let Some(i) = addrs.iter().position(|a| *a == p.addr)
                {
                    addrs[..=i].rotate_right(1);
                }
                (k.clone(), addrs)
            })
            .collect();
        let answers: Vec<(String, Option<(SocketAddr, Info)>)> = thread::scope(|scope| {
            let handles: Vec<_> = targets
                .iter()
                .map(|(k, addrs)| (k, scope.spawn(|| ask(addrs))))
                .collect();
            handles
                .into_iter()
                .map(|(k, h)| (k.clone(), h.join().unwrap_or(None)))
                .collect()
        });
        known.retain(|k, _| targets.iter().any(|(t, _)| t == k));
        for (key, answer) in answers {
            match answer {
                Some((addr, info)) => {
                    let peer = Peer {
                        id: info.id,
                        name: info.name,
                        addr,
                        version: info.version,
                        shares: info.shares,
                    };
                    known.insert(key, (peer, 0));
                }
                None => {
                    if let Some((_, misses)) = known.get_mut(&key) {
                        *misses += 1;
                    }
                }
            }
        }
        let peers = visible_peers(&s.my_id, known.values());
        if last.as_ref() != Some(&peers) {
            on_change(peers.clone());
            last = Some(peers);
        }
        match wake.recv_timeout(POLL_EVERY) {
            Ok(()) => {
                // Let a burst of mDNS events settle into one poll.
                thread::sleep(Duration::from_millis(200));
                while wake.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// One entry per instance (it may be known by mDNS and a static address),
/// not ourselves, sorted by name.
fn visible_peers<'a>(my_id: &str, known: impl Iterator<Item = &'a (Peer, u32)>) -> Vec<Peer> {
    let mut by_id: HashMap<&str, &Peer> = HashMap::new();
    for (p, misses) in known {
        if p.id == my_id || *misses >= MISSES_TO_HIDE {
            continue;
        }
        let better = by_id
            .get(p.id.as_str())
            .is_none_or(|q| address_rank(&p.addr) < address_rank(&q.addr));
        if better {
            by_id.insert(&p.id, p);
        }
    }
    let mut out: Vec<Peer> = by_id.into_values().cloned().collect();
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Server, State};
    use std::net::Ipv4Addr;
    use std::time::Instant;

    fn peer(id: &str, name: &str, addr: &str) -> Peer {
        Peer {
            id: id.into(),
            name: name.into(),
            addr: addr.parse().unwrap(),
            version: "1".into(),
            shares: vec![],
        }
    }

    #[test]
    fn ranks_and_merges() {
        let known = [
            (peer("b", "beta", "172.17.0.1:1"), 0),
            (peer("b", "beta", "192.168.1.2:1"), 0),
            (peer("me", "me", "192.168.1.3:1"), 0),
            (peer("a", "Alpha", "10.0.0.1:1"), 1),
            (peer("gone", "gone", "10.0.0.2:1"), MISSES_TO_HIDE),
        ];
        let peers = visible_peers("me", known.iter());
        let got: Vec<_> = peers
            .iter()
            .map(|p| (p.id.as_str(), p.addr.to_string()))
            .collect();
        assert_eq!(
            got,
            [
                ("a", "10.0.0.1:1".to_string()),
                ("b", "192.168.1.2:1".to_string())
            ]
        );
    }

    fn wait_for(rx: &mpsc::Receiver<Vec<Peer>>, f: impl Fn(&[Peer]) -> bool) -> Vec<Peer> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let peers = rx.recv_timeout(left).expect("timed out waiting for peers");
            if f(&peers) {
                return peers;
            }
        }
    }

    #[test]
    fn static_peer_appears_changes_and_vanishes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::new(
            "other",
            "Other",
            crate::shares::from_paths(&[tmp.path().to_path_buf()]),
        );
        let server = Server::start(state.clone(), 0).unwrap();
        let (tx, rx) = mpsc::channel();
        let cfg = DiscoveryConfig {
            id: "me".into(),
            name: "Me".into(),
            advertise_port: None,
            mdns: false,
            static_peers: vec![format!("127.0.0.1:{}", server.port())],
        };
        let d = Discovery::start(cfg, move |p| {
            let _ = tx.send(p);
        })
        .unwrap();
        let peers = wait_for(&rx, |p| p.len() == 1);
        assert_eq!(peers[0].name, "Other");
        assert_eq!(
            peers[0].addr,
            SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()))
        );

        state.set_shares(vec![]);
        d.refresh();
        wait_for(&rx, |p| p.len() == 1 && p[0].shares.is_empty());

        drop(server);
        d.refresh();
        wait_for(&rx, |p| p.is_empty());
    }

    /// Real multicast; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn finds_each_other_over_mdns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut instances = Vec::new();
        let mut receivers = Vec::new();
        for (id, name) in [("mdns-test-a", "A"), ("mdns-test-b", "B")] {
            let state = State::new(
                id,
                name,
                crate::shares::from_paths(&[tmp.path().to_path_buf()]),
            );
            let server = Server::start(state, 0).unwrap();
            let (tx, rx) = mpsc::channel();
            let cfg = DiscoveryConfig {
                id: id.into(),
                name: name.into(),
                advertise_port: Some(server.port()),
                mdns: true,
                static_peers: vec![],
            };
            let d = Discovery::start(cfg, move |p| {
                let _ = tx.send(p);
            })
            .unwrap();
            instances.push((server, d));
            receivers.push(rx);
        }
        wait_for(&receivers[0], |p| p.iter().any(|p| p.id == "mdns-test-b"));
        wait_for(&receivers[1], |p| p.iter().any(|p| p.id == "mdns-test-a"));
    }
}
