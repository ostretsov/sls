//! sls core: everything that works without a display.
//!
//! * [`config`] — settings in `~/.config/sls/config.toml`
//! * [`shares`] — the shared folders and safe path resolution inside them
//! * [`http`] — a tiny HTTP/1.1 server and client (std only)
//! * [`server`] — what an instance serves to the LAN (JSON API + browser pages)
//! * [`client`] — listing and downloading from another instance
//! * [`discovery`] — finding other instances (mDNS + polling their `/api/info`)
//! * [`autostart`] — the "start at login" desktop entry

pub mod autostart;
pub mod client;
pub mod config;
pub mod discovery;
pub mod http;
pub mod protocol;
pub mod server;
pub mod shares;

/// "1.5 MB" style sizes (decimal units, like GNOME Files).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(999), "999 bytes");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(1_500_000), "1.5 MB");
        assert_eq!(human_size(42_000_000_000), "42 GB");
    }
}
