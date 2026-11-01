//! Terminal modes: `--serve` and `--peers`.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::bail;
use sls_core::config::Config;
use sls_core::discovery::{Discovery, DiscoveryConfig, Peer};
use sls_core::server::{Server, State};
use sls_core::shares;

fn print_peers(peers: &[Peer]) {
    if peers.is_empty() {
        println!("No other computers found.");
    }
    for p in peers {
        println!("{} (http://{}/)", p.name, p.addr);
        if p.shares.is_empty() {
            println!("    (shares nothing)");
        }
        for s in &p.shares {
            println!("    {s}");
        }
    }
}

pub fn serve(config_dir: &Path, dirs: Vec<PathBuf>) -> anyhow::Result<()> {
    let cfg = Config::load(config_dir)?;
    let folders = if dirs.is_empty() {
        cfg.shares.clone()
    } else {
        dirs
    };
    let folders: Vec<PathBuf> = folders
        .into_iter()
        .map(|d| {
            d.canonicalize()
                .map_err(|e| anyhow::anyhow!("{}: {e}", d.display()))
        })
        .collect::<Result<_, _>>()?;
    if folders.is_empty() {
        bail!("nothing to share: name folders (sls --serve DIR...) or choose them in the window");
    }
    let shares = shares::from_paths(&folders);
    let state = State::new(&cfg.id, &cfg.name, shares.clone());
    let server = Server::start_with_fallback(state, cfg.port)?;
    println!("Sharing as \"{}\" on port {}:", cfg.name, server.port());
    for s in &shares {
        println!("    {}  ({})", s.name, s.path.display());
    }
    println!(
        "Browsers: http://{}:{}/",
        crate::ui::local_ip(),
        server.port()
    );
    let (tx, rx) = mpsc::channel();
    let _discovery = Discovery::start(
        DiscoveryConfig {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            advertise_port: Some(server.port()),
            mdns: cfg.discovery,
            static_peers: cfg.peers.clone(),
        },
        move |p| {
            let _ = tx.send(p);
        },
    )?;
    for peers in rx {
        println!("\n--- Others on the network:");
        print_peers(&peers);
    }
    Ok(())
}

pub fn peers(config_dir: &Path, secs: u64) -> anyhow::Result<()> {
    let cfg = Config::load(config_dir)?;
    let (tx, rx) = mpsc::channel();
    let _discovery = Discovery::start(
        DiscoveryConfig {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            advertise_port: None,
            mdns: true,
            static_peers: cfg.peers.clone(),
        },
        move |p| {
            let _ = tx.send(p);
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut latest = Vec::new();
    while let Ok(p) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        latest = p;
    }
    print_peers(&latest);
    Ok(())
}
