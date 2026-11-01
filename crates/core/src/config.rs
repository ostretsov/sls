//! Settings: `~/.config/sls/config.toml` (or `--config-dir DIR`).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 47300;
const FILE: &str = "config.toml";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Random id of this instance (generated once).
    pub id: String,
    /// Name shown to the others (default: the host name).
    pub name: String,
    /// TCP port of the built-in web server (another free one is used if taken).
    pub port: u16,
    /// Find other instances with mDNS (and be found).
    pub discovery: bool,
    /// Where downloads go (default: the XDG download folder, ~/Downloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
    /// Instances to ask directly, "host:port", for networks without multicast.
    pub peers: Vec<String>,
    /// While the window is closed, keep sharing in the background.
    pub run_in_background: bool,
    /// The shared folders.
    pub shares: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            id: String::new(),
            name: String::new(),
            port: DEFAULT_PORT,
            discovery: true,
            download_dir: None,
            peers: Vec::new(),
            run_in_background: false,
            shares: Vec::new(),
        }
    }
}

impl Config {
    /// Loads the settings (defaults if there are none yet) and fills in a new
    /// id and the host name when missing; a new id is saved right away.
    pub fn load(dir: &Path) -> anyhow::Result<Config> {
        let path = dir.join(FILE);
        let mut cfg: Config = match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
        };
        if cfg.name.trim().is_empty() {
            cfg.name = hostname();
        }
        if cfg.id.is_empty() {
            cfg.id = random_id();
            cfg.save(dir)?;
        }
        Ok(cfg)
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        fs::write(&tmp, toml::to_string(self)?)?;
        fs::rename(&tmp, dir.join(FILE))?;
        Ok(())
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir.clone().unwrap_or_else(xdg_download_dir)
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg_config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".config"),
    }
}

pub fn default_dir() -> PathBuf {
    xdg_config_home().join("sls")
}

/// `~/Downloads`, or what `~/.config/user-dirs.dirs` says (localized desktops).
pub fn xdg_download_dir() -> PathBuf {
    let text = fs::read_to_string(xdg_config_home().join("user-dirs.dirs")).unwrap_or_default();
    parse_download_dir(&text, &home()).unwrap_or_else(|| home().join("Downloads"))
}

fn parse_download_dir(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    let line = user_dirs
        .lines()
        .find(|l| l.trim_start().starts_with("XDG_DOWNLOAD_DIR="))?;
    let value = line.split_once('=')?.1.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // "$HOME/" alone means the desktop has no download folder.
    (path.is_absolute() && path != home).then_some(path)
}

pub fn hostname() -> String {
    let name = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    match name.trim() {
        "" => "sls".into(),
        n => n.into(),
    }
}

/// 16 hex digits from /dev/urandom.
pub fn random_id() -> String {
    let mut bytes = [0u8; 8];
    let ok = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if ok.is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        bytes = ((t.as_nanos() as u64) ^ ((std::process::id() as u64) << 32)).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_gets_id_and_name_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.id.len(), 16);
        assert!(!cfg.name.is_empty());
        assert_eq!(cfg.port, DEFAULT_PORT);
        // The id is stable.
        assert_eq!(Config::load(dir.path()).unwrap().id, cfg.id);

        cfg.shares = vec!["/srv/music".into(), "/home/me/Photos".into()];
        cfg.peers = vec!["192.168.1.5:47300".into()];
        cfg.run_in_background = true;
        cfg.save(dir.path()).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), cfg);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE),
            "name = \"Kitchen PC\"\nport = 5000\n",
        )
        .unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.name, "Kitchen PC");
        assert_eq!(cfg.port, 5000);
        assert!(cfg.discovery);
        assert!(cfg.shares.is_empty());
    }

    #[test]
    fn broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), "port = \"x\"").unwrap();
        assert!(Config::load(dir.path()).is_err());
    }

    #[test]
    fn download_dir_from_user_dirs() {
        let home = Path::new("/home/u");
        let text =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Descargas\"\n";
        assert_eq!(
            parse_download_dir(text, home),
            Some("/home/u/Descargas".into())
        );
        assert_eq!(
            parse_download_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", home),
            Some("/data/dl".into())
        );
        assert_eq!(
            parse_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", home),
            None
        );
        assert_eq!(parse_download_dir("", home), None);
    }
}
