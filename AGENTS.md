# Notes for coding agents

sls (simple LAN share): share folders over the LAN, find other instances with
mDNS (Rust, GTK 4, Ubuntu). `README.md` describes the features; this file is
about working on the code.

## Layout

- `crates/core` — everything testable without a display: config, shares and
  safe path resolution (`shares.rs`), a tiny std-only HTTP server/client
  (`http.rs`), the routes (`server.rs`), listing/downloading (`client.rs`),
  discovery (`discovery.rs`: mDNS + polling `/api/info`), autostart entry.
  Put new logic here, with unit tests.
- `crates/app` — `ui.rs` (GTK window, background mode), `cli.rs` (`--serve`,
  `--peers`), `selftest.rs` (`--self-test`, used by the Docker test).

## Build and test

- `make help` lists everything. `make test` and `make lint` (fmt + clippy
  with `-D warnings`) must pass before you finish.
- `make lint` only *checks*; run `make build` before running
  `target/debug/sls`, or you test the previous binary.
- Free disk space is tight on this machine: check `df -h /` before big builds.

## Running test instances

- Always pass `--config-dir <scratch>` (it also makes the GTK app
  non-unique, so it does not hand over to the user's real running sls) and a
  different `port` in that config.
- Test instances advertise themselves on the user's real LAN. Stop them when
  done: `pkill -x sls` (not `pkill -f`, which matches your own shell).
- Set `XDG_CONFIG_HOME` to a scratch dir before touching the "Start at login"
  switch, or it writes the user's real `~/.config/autostart/sls.desktop`.
- Look at the GUI on a private display, never the user's desktop:
  `Xvfb :77 &`, `DISPLAY=:77 sls --config-dir …`, `import -window root x.png`.
  Clicking with XTest is OK there (only there).
- `sls --serve DIR` + `sls --peers` test discovery and sharing without a GUI.
- Start background processes with `setsid … & disown`. The shell is zsh: put
  multi-step scripts in a file and run them with `bash`.

## Packaging

- `make deb` builds the self-contained package (`scripts/build-bundle-deb.sh`):
  everything except the libraries matched by `EXCLUDE` is copied into
  `/opt/sls/lib`; `Depends` is derived with `dpkg -S` from the excluded ones.
  GTK's gsettings schemas are bundled (the folder chooser aborts without
  them). The launcher sets `LD_LIBRARY_PATH` etc.; `ui::open_external`
  removes them again for xdg-open.
- `make docker-test` installs the `.deb` in clean Ubuntu containers and runs
  `sls --self-test`. Run it after packaging changes.
- Releases: push a `vX.Y.Z` tag matching `crates/app/Cargo.toml`
  (`make version V=X.Y.Z`); `.github/workflows/release.yml` does the rest.
