# sls — simple LAN share

Share folders with the other computers on your local network. No accounts, no
servers, no addresses to type.

* **Pick folders to share.** Click **Share folders…** (select several at once)
  or drag folders from Files into the window. **−** stops sharing a folder.
  Shares are read-only: the others can look and download, never change or
  delete.
* **Finds the others by itself.** Every computer running sls on the same
  network appears under **On the network** with the folders it shares (mDNS,
  like printers). The list updates by itself within a few seconds when someone
  starts, stops or changes what they share.
* **Browse and download.** Click a shared folder to browse it. Click a folder
  to open it, click a file (or ↓) to download it, **Download this folder**
  takes everything below the current folder. Downloads go to your Downloads
  folder; nothing is ever overwritten (`photo (1).jpg`). **Show** opens the
  result in Files.
* **Send someone a file.** Click the send button next to a computer under
  **On the network** and pick a file in one of your shared folders. They get
  *“Alice wants to send you report.pdf”* with **Accept** and **Decline**
  (and a desktop notification); **Accept** downloads it like any other
  download. You see their answer. Only files in shared folders can be sent,
  because that is what the others can download anyway.
* **Phones and computers without sls** use a web browser: the address is shown
  under your shares (e.g. `http://192.168.1.20:47300/`). Videos and music
  play directly in the browser (seeking works).
* **Start at login.** Turn on *Start at login and keep sharing when the window
  is closed*: sls then starts without a window when you log in and keeps
  sharing in the background.
* **Top bar icon.** While sls runs, its icon is in the top bar (Ubuntu's
  AppIndicator extension, on by default). Click it to open the window; its
  menu also has **Quit**. When someone offers you a file the icon gets an
  orange badge until you answer. sls is also in the app menu (Activities).
* **Your name** is what the others see (default: the computer's name).

Anyone on your local network can read what you share. Don't share private
folders on public Wi-Fi.

## Install (Ubuntu 22.04 or newer)

Download `sls_<version>_amd64.deb` from the
[releases](https://github.com/ostretsov/sls/releases) and install it:

```sh
sudo apt install ./sls_0.2.0_amd64.deb
```

The package is self-contained: GTK 4 and its libraries are bundled in
`/opt/sls`, so it does not depend on the GTK version of your Ubuntu.

If two computers don't see each other, the firewall probably blocks them
(Ubuntu has none enabled by default). sls needs TCP port 47300 and mDNS
(UDP 5353):

```sh
sudo ufw allow 47300/tcp
sudo ufw allow 5353/udp
```

Networks that block multicast (some office or guest Wi-Fi) can list the other
computers by hand in the settings, see below.

## Settings

`~/.config/sls/config.toml` (the window keeps it up to date):

```toml
name = "Living room PC"      # what the others see
port = 47300                 # web server port (another free one if taken)
discovery = true             # mDNS
download_dir = "/data/inbox" # default: your Downloads folder
peers = ["192.168.1.7:47300"]   # computers to ask directly (no multicast)
run_in_background = false
shares = ["/home/me/Music", "/home/me/Photos"]
```

## Terminal use

```sh
sls --serve ~/Music ~/Videos   # share without a window (Ctrl+C stops)
sls --peers                    # who is on the network and what they share
sls --self-test                # check that everything works
sls --help
```

## Building

Needs Rust ([rustup.rs](https://rustup.rs)) and the GTK 4 headers:

```sh
make deps      # once: build dependencies (sudo)
make run       # try it
make test      # tests (make test-all also tries real mDNS)
make deb       # target/sls_0.2.0_amd64.deb (self-contained)
make demo      # two instances on this machine that see each other
make help      # everything else
```

## Releases

GitHub Actions (`.github/workflows/release.yml`) builds the `.deb` on Ubuntu
22.04, installs it in clean Ubuntu 22.04/24.04/rolling containers, runs
`sls --self-test` there and attaches it to a GitHub release:

```sh
make version V=0.2.0
git commit -am "Release 0.2.0"
git tag v0.2.0
git push --follow-tags
```

## How it works

Each instance runs a small read-only web server and advertises it over mDNS
as `_sls._tcp`. The others poll its `/api/info` (name and share names) every
few seconds, list folders with `/api/list/<share>/<path>` and download from
`/f/<share>/<path>` (HTTP ranges supported). Requests can only reach files
inside the shared folders: `..` is refused and symlinks leading outside a
share are neither listed nor served.

Sending a file is an offer: `POST /api/offer` names a file in one of the
sender's shares. If the receiver accepts, it downloads the file from the
address the offer came from, never from an address the offer names, and
answers with `POST /api/offer-reply`. Nothing is ever written to a
computer without the receiver accepting. At most 20 offers can wait at
once.

## License

MIT
