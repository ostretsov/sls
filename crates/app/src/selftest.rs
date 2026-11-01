//! `sls --self-test`: serve a temporary folder, download it back over
//! loopback, then check that GTK (incl. the folder chooser's settings schema
//! and the icon loaders bundled in the .deb) works.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use gtk::prelude::*;
use sls_core::server::{Server, State};
use sls_core::{client, shares};

pub fn run() -> anyhow::Result<()> {
    let tmp = std::env::temp_dir().join(format!("sls-self-test-{}", std::process::id()));
    let result = network(&tmp);
    let _ = fs::remove_dir_all(&tmp);
    result?;
    println!("sharing and downloading: OK");
    gui()?;
    println!("self-test OK");
    Ok(())
}

fn network(tmp: &std::path::Path) -> anyhow::Result<()> {
    let src = tmp.join("Shared");
    fs::create_dir_all(src.join("sub"))?;
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
    fs::write(src.join("sub/data.bin"), &data)?;
    fs::write(src.join("Ñandú.txt"), "hola")?;

    let state = State::new(
        "self-test",
        "self-test",
        shares::from_paths(std::slice::from_ref(&src)),
    );
    let server = Server::start(state, 0).context("starting the web server")?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
    let info = client::fetch_info(addr, Duration::from_secs(3))?;
    ensure!(
        info.shares == ["Shared"],
        "unexpected shares {:?}",
        info.shares
    );
    let dest = tmp.join("downloads");
    let got = client::download(
        addr,
        "Shared",
        &[],
        true,
        &dest,
        &AtomicBool::new(false),
        &mut |_| (),
    )?;
    ensure!(
        fs::read(got.join("sub/data.bin"))? == data,
        "downloaded file differs"
    );
    ensure!(
        fs::read_to_string(got.join("Ñandú.txt"))? == "hola",
        "downloaded file differs"
    );
    Ok(())
}

fn gui() -> anyhow::Result<()> {
    gtk::init().context("GTK cannot start (no display?)")?;
    // Needs the org.gtk.gtk4.Settings.FileChooser schema.
    let chooser = gtk::FileChooserWidget::new(gtk::FileChooserAction::SelectFolder);
    chooser.set_select_multiple(true);
    let window = gtk::Window::new();
    window.set_child(Some(&chooser));
    window.present();
    let display = gtk::gdk::Display::default().context("no display")?;
    let theme = gtk::IconTheme::for_display(&display);
    for icon in [
        "folder-symbolic",
        "folder-remote-symbolic",
        "computer-symbolic",
    ] {
        if !theme.has_icon(icon) {
            bail!("icon {icon} not found (is adwaita-icon-theme installed?)");
        }
    }
    let ctx = gtk::glib::MainContext::default();
    for _ in 0..50 {
        ctx.iteration(false);
    }
    window.destroy();
    println!("GTK and the folder chooser: OK");
    Ok(())
}
