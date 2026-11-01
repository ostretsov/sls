use std::path::PathBuf;
use std::process::ExitCode;

mod cli;
mod selftest;
mod ui;

const HELP: &str = "\
sls — simple LAN share: share folders with other computers on the local network

Usage:
  sls                     open the window
  sls --background        start without a window (used by \"Start at login\");
                          starting sls again shows the window
  sls --serve [DIR...]    share without a window, in the terminal (default: the
                          folders chosen in the window); Ctrl+C stops
  sls --peers [SECONDS]   list the other computers and what they share, then exit
  sls --self-test         check that sharing, downloading and GTK work
  sls --version

Options:
  --config-dir DIR        settings folder (default ~/.config/sls); allows
                          several instances on one computer
";

#[derive(Debug, PartialEq)]
enum Mode {
    Window { background: bool },
    Serve(Vec<PathBuf>),
    Peers(u64),
    SelfTest,
    Help,
    Version,
}

#[derive(Debug, PartialEq)]
struct Args {
    mode: Mode,
    config_dir: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut mode = Mode::Window { background: false };
    let mut config_dir = None;
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config-dir" => {
                config_dir = Some(PathBuf::from(
                    it.next().ok_or("--config-dir needs a folder")?,
                ))
            }
            "--background" => mode = Mode::Window { background: true },
            "--self-test" => mode = Mode::SelfTest,
            "-h" | "--help" => mode = Mode::Help,
            "-V" | "--version" => mode = Mode::Version,
            "--peers" => {
                let secs = match it.peek() {
                    Some(s) if !s.starts_with('-') => {
                        let s = it.next().unwrap();
                        s.parse()
                            .map_err(|_| format!("--peers: not a number of seconds: {s}"))?
                    }
                    _ => 4,
                };
                mode = Mode::Peers(secs);
            }
            "--serve" => {
                let mut dirs = Vec::new();
                while let Some(d) = it.next_if(|s| !s.starts_with('-')) {
                    dirs.push(PathBuf::from(d));
                }
                mode = Mode::Serve(dirs);
            }
            other => return Err(format!("unknown argument: {other} (see sls --help)")),
        }
    }
    Ok(Args { mode, config_dir })
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sls: {e}");
            return ExitCode::from(2);
        }
    };
    let custom_dir = args.config_dir.is_some();
    let config_dir = args
        .config_dir
        .unwrap_or_else(sls_core::config::default_dir);
    let result = match args.mode {
        Mode::Help => {
            print!("{HELP}");
            Ok(())
        }
        Mode::Version => {
            println!("sls {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Mode::SelfTest => selftest::run(),
        Mode::Serve(dirs) => cli::serve(&config_dir, dirs),
        Mode::Peers(secs) => cli::peers(&config_dir, secs),
        Mode::Window { background } => {
            let code = ui::run(ui::Options {
                config_dir,
                custom_config_dir: custom_dir,
                background,
            });
            return if code == gtk::glib::ExitCode::SUCCESS {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sls: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn arguments() {
        assert_eq!(parse("").unwrap().mode, Mode::Window { background: false });
        assert_eq!(
            parse("--background").unwrap().mode,
            Mode::Window { background: true }
        );
        let a = parse("--serve /a /b --config-dir /c").unwrap();
        assert_eq!(a.mode, Mode::Serve(vec!["/a".into(), "/b".into()]));
        assert_eq!(a.config_dir, Some("/c".into()));
        assert_eq!(parse("--serve").unwrap().mode, Mode::Serve(vec![]));
        assert_eq!(parse("--peers").unwrap().mode, Mode::Peers(4));
        assert_eq!(parse("--peers 9").unwrap().mode, Mode::Peers(9));
        assert!(parse("--peers x").is_err());
        assert!(parse("--config-dir").is_err());
        assert!(parse("--bogus").is_err());
    }
}
