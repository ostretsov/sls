//! The window: your shared folders on the left, the others' below them, and
//! a browser for the selected share on the right.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};

use sls_core::client::{self, Progress};
use sls_core::config::Config;
use sls_core::discovery::{Discovery, DiscoveryConfig, Peer};
use sls_core::protocol::Entry;
use sls_core::server::{Server, State};
use sls_core::{autostart, human_size, shares};

pub const APP_ID: &str = "io.github.ostretsov.sls";

pub struct Options {
    pub config_dir: PathBuf,
    pub custom_config_dir: bool,
    pub background: bool,
}

enum Ev {
    Peers(Vec<Peer>),
    Listing {
        seq: u64,
        result: Result<Vec<Entry>, String>,
    },
    DlProgress {
        id: u64,
        progress: Progress,
    },
    DlDone {
        id: u64,
        result: Result<PathBuf, String>,
    },
}

#[derive(Clone, Debug)]
struct Location {
    peer_id: String,
    peer_name: String,
    addr: SocketAddr,
    share: String,
    path: Vec<String>,
}

struct DlRow {
    widget: gtk::Box,
    label: gtk::Label,
    bar: gtk::ProgressBar,
    action: gtk::Button,
    dismiss: gtk::Button,
    cancel: Arc<AtomicBool>,
    name: String,
    saved: Option<PathBuf>,
}

struct Ui {
    opts: Options,
    cfg: RefCell<Config>,
    state: Arc<State>,
    server: RefCell<Option<Server>>,
    discovery: RefCell<Option<Discovery>>,
    tx: async_channel::Sender<Ev>,

    window: gtk::ApplicationWindow,
    my_list: gtk::ListBox,
    net_list: gtk::ListBox,
    /// Per row of `net_list`: the (peer id, share) it opens.
    net_rows: RefCell<Vec<Option<(String, String)>>>,
    peers: RefCell<Vec<Peer>>,

    loc: RefCell<Option<Location>>,
    crumb: gtk::Label,
    up: gtk::Button,
    reload: gtk::Button,
    dl_here: gtk::Button,
    entries_list: gtk::ListBox,
    entries_placeholder: gtk::Label,
    entries: RefCell<Vec<Entry>>,
    seq: Cell<u64>,

    downloads_box: gtk::Box,
    downloads_scroll: gtk::ScrolledWindow,
    downloads: RefCell<HashMap<u64, DlRow>>,
    next_dl: Cell<u64>,
    chooser: RefCell<Option<gtk::FileChooserNative>>,
}

pub fn run(opts: Options) -> glib::ExitCode {
    let mut flags = gio::ApplicationFlags::empty();
    if opts.custom_config_dir {
        // Test instances next to the real one.
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    if let Err(e) = app.register(gio::Cancellable::NONE) {
        eprintln!("sls: {e}");
        return glib::ExitCode::FAILURE;
    }
    if app.is_remote() {
        // Already running.  The autostart leaves it alone; a normal start
        // shows its window (`run` sends "activate" to it and returns).
        if opts.background {
            std::process::exit(0);
        }
        return app.run_with_args(&[std::env::args().next().unwrap_or_default()]);
    }
    let background = opts.background;
    let ui = match build(&app, opts) {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("sls: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    // `run` activates once; in background mode that one stays hidden.
    let skip = Cell::new(background);
    let u = ui.clone();
    app.connect_activate(move |_| {
        if !skip.replace(false) {
            u.window.present();
        }
    });
    let u = ui.clone();
    app.connect_shutdown(move |_| {
        // Say goodbye on the network.
        u.discovery.borrow_mut().take();
        u.server.borrow_mut().take();
    });
    let argv0 = std::env::args().next().unwrap_or_else(|| "sls".into());
    app.run_with_args(&[argv0])
}

/// The address others reach this computer at (for the browser hint).
pub fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            // No packet is sent: this only asks the kernel which source
            // address it would use.
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| format!("{}.local", sls_core::config::hostname()))
}

/// Opens a folder or URL with the desktop's default app.  The .deb's
/// launcher points GTK at bundled libraries; the opened app must not inherit that.
fn open_external(target: &str) {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(target);
    if std::env::var_os("SLS_BUNDLE").is_some() {
        for v in [
            "LD_LIBRARY_PATH",
            "GIO_MODULE_DIR",
            "GDK_PIXBUF_MODULE_FILE",
            "GSETTINGS_SCHEMA_DIR",
            "SLS_BUNDLE",
        ] {
            cmd.env_remove(v);
        }
        if let Some(orig) = std::env::var_os("SLS_ORIG_LD_LIBRARY_PATH").filter(|v| !v.is_empty()) {
            cmd.env("LD_LIBRARY_PATH", orig);
        }
        cmd.env_remove("SLS_ORIG_LD_LIBRARY_PATH");
    }
    if let Err(e) = cmd.spawn() {
        eprintln!("sls: cannot run xdg-open: {e}");
    }
}

/// The command the autostart entry runs.
fn exec_path() -> String {
    if Path::new("/usr/bin/sls").exists() {
        return "sls".into();
    }
    std::env::current_exe().map_or_else(|_| "sls".into(), |p| p.display().to_string())
}

fn markup_label(markup: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_markup(markup);
    l.set_xalign(0.0);
    l
}

fn heading(text: &str) -> gtk::Label {
    let l = markup_label(&format!("<b>{}</b>", glib::markup_escape_text(text)));
    l.set_margin_top(12);
    l
}

fn dim(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.add_css_class("dim-label");
    l.set_ellipsize(pango::EllipsizeMode::Middle);
    l
}

fn placeholder(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dim-label");
    l.set_wrap(true);
    l.set_justify(gtk::Justification::Center);
    l.set_margin_top(18);
    l.set_margin_bottom(18);
    l.set_margin_start(12);
    l.set_margin_end(12);
    l
}

fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

fn margins(w: &impl IsA<gtk::Widget>, m: i32) {
    w.set_margin_top(m);
    w.set_margin_bottom(m);
    w.set_margin_start(m);
    w.set_margin_end(m);
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.set_valign(gtk::Align::Center);
    b.add_css_class("flat");
    b
}

fn clear(list: &gtk::ListBox) {
    // Not `first_child`: that would also remove the placeholder.
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
}

fn build(app: &gtk::Application, opts: Options) -> anyhow::Result<Rc<Ui>> {
    let cfg = Config::load(&opts.config_dir)?;
    let state = State::new(&cfg.id, &cfg.name, shares::from_paths(&cfg.shares));
    let server = Server::start_with_fallback(state.clone(), cfg.port)?;
    let port = server.port();
    let (tx, rx) = async_channel::unbounded::<Ev>();

    let dtx = tx.clone();
    let discovery = Discovery::start(
        DiscoveryConfig {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            advertise_port: Some(port),
            mdns: cfg.discovery,
            static_peers: cfg.peers.clone(),
        },
        move |peers| {
            let _ = dtx.send_blocking(Ev::Peers(peers));
        },
    );
    let (discovery, discovery_error) = match discovery {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };

    // ---- left: me + the network
    let name_entry = gtk::Entry::builder()
        .text(cfg.name.as_str())
        .hexpand(true)
        .build();
    let name_row = hbox(8);
    name_row.append(&gtk::Label::new(Some("Your name:")));
    name_row.append(&name_entry);

    let my_list = gtk::ListBox::new();
    my_list.set_selection_mode(gtk::SelectionMode::None);
    my_list.add_css_class("boxed-list");
    my_list.set_placeholder(Some(&placeholder(
        "Nothing shared yet.\nClick “Share folders…” or drop folders here.",
    )));
    let add = gtk::Button::with_label("Share folders…");
    add.set_halign(gtk::Align::Start);

    let url = format!("http://{}:{port}/", local_ip());
    let hint = markup_label(&format!(
        "<small>Read-only for everyone on your network. Web browsers (phones too): <a href=\"{0}\">{0}</a></small>",
        glib::markup_escape_text(&url)
    ));
    hint.set_wrap(true);
    hint.connect_activate_link(|_, uri| {
        open_external(uri);
        glib::Propagation::Stop
    });

    let bg_switch = gtk::Switch::new();
    bg_switch.set_active(cfg.run_in_background);
    bg_switch.set_valign(gtk::Align::Center);
    let bg_label = gtk::Label::new(Some(
        "Start at login and keep sharing when the window is closed",
    ));
    bg_label.set_wrap(true);
    bg_label.set_xalign(0.0);
    bg_label.set_hexpand(true);
    let bg_row = hbox(8);
    bg_row.append(&bg_label);
    bg_row.append(&bg_switch);

    let net_list = gtk::ListBox::new();
    net_list.set_selection_mode(gtk::SelectionMode::Single);
    net_list.add_css_class("boxed-list");
    let net_placeholder = placeholder(match &discovery_error {
        Some(_) => "Cannot look for other computers (see the terminal output).",
        None => "Looking for other computers running sls…",
    });
    if let Some(e) = &discovery_error {
        eprintln!("sls: discovery: {e}");
    }
    net_list.set_placeholder(Some(&net_placeholder));

    let left = vbox(8);
    margins(&left, 12);
    left.append(&name_row);
    left.append(&heading("Shared by you"));
    left.append(&my_list);
    left.append(&add);
    left.append(&hint);
    left.append(&bg_row);
    left.append(&heading("On the network"));
    left.append(&net_list);
    let left_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&left)
        .build();

    // ---- right: browser + downloads
    let up = icon_button("go-up-symbolic", "Up one folder");
    let crumb = gtk::Label::new(None);
    crumb.set_xalign(0.0);
    crumb.set_hexpand(true);
    crumb.set_ellipsize(pango::EllipsizeMode::Start);
    let reload = icon_button("view-refresh-symbolic", "Reload");
    let dl_here = gtk::Button::with_label("Download this folder");
    let bar = hbox(6);
    margins(&bar, 6);
    bar.append(&up);
    bar.append(&crumb);
    bar.append(&reload);
    bar.append(&dl_here);

    let entries_list = gtk::ListBox::new();
    entries_list.set_selection_mode(gtk::SelectionMode::None);
    let entries_placeholder = placeholder("Pick a shared folder on the left.");
    entries_list.set_placeholder(Some(&entries_placeholder));
    let entries_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&entries_list)
        .build();

    let downloads_box = vbox(6);
    margins(&downloads_box, 8);
    let downloads_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(220)
        .propagate_natural_height(true)
        .child(&downloads_box)
        .visible(false)
        .build();

    let right = vbox(0);
    right.append(&bar);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&entries_scroll);
    right.append(&downloads_scroll);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&left_scroll));
    paned.set_end_child(Some(&right));
    paned.set_position(380);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);

    let header = gtk::HeaderBar::new();
    let quit = gtk::Button::with_label("Quit");
    quit.set_tooltip_text(Some("Stop sharing and close sls"));
    header.pack_end(&quit);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("sls — {}", cfg.name))
        .default_width(1100)
        .default_height(700)
        .child(&paned)
        .build();
    window.set_titlebar(Some(&header));
    window.set_hide_on_close(cfg.run_in_background);

    let ui = Rc::new(Ui {
        opts,
        cfg: RefCell::new(cfg),
        state,
        server: RefCell::new(Some(server)),
        discovery: RefCell::new(discovery),
        tx,
        window,
        my_list,
        net_list,
        net_rows: RefCell::new(Vec::new()),
        peers: RefCell::new(Vec::new()),
        loc: RefCell::new(None),
        crumb,
        up,
        reload,
        dl_here,
        entries_list,
        entries_placeholder,
        entries: RefCell::new(Vec::new()),
        seq: Cell::new(0),
        downloads_box,
        downloads_scroll,
        downloads: RefCell::new(HashMap::new()),
        next_dl: Cell::new(1),
        chooser: RefCell::new(None),
    });

    // ---- signals
    let u = ui.clone();
    name_entry.connect_changed(move |e| {
        let name = e.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        u.state.set_name(&name);
        u.window.set_title(Some(&format!("sls — {name}")));
        u.cfg.borrow_mut().name = name;
        u.save();
    });
    let u = ui.clone();
    add.connect_clicked(move |_| u.choose_folders());
    let u = ui.clone();
    bg_switch.connect_state_set(move |_, on| {
        u.set_background(on);
        glib::Propagation::Proceed
    });
    let u = ui.clone();
    quit.connect_clicked(move |_| {
        if let Some(app) = u.window.application() {
            app.quit();
        }
    });
    let u = ui.clone();
    ui.net_list.connect_row_activated(move |_, row| {
        let key = u
            .net_rows
            .borrow()
            .get(row.index() as usize)
            .cloned()
            .flatten();
        if let Some((peer_id, share)) = key {
            let peer = u.peers.borrow().iter().find(|p| p.id == peer_id).cloned();
            if let Some(p) = peer {
                u.open(Location {
                    peer_id,
                    peer_name: p.name,
                    addr: p.addr,
                    share,
                    path: vec![],
                });
            }
        }
    });
    let u = ui.clone();
    ui.entries_list.connect_row_activated(move |_, row| {
        let entry = u.entries.borrow().get(row.index() as usize).cloned();
        if let Some(e) = entry {
            u.entry_chosen(&e, false);
        }
    });
    let u = ui.clone();
    ui.up.connect_clicked(move |_| {
        let loc = u.loc.borrow().clone();
        if let Some(mut loc) = loc
            && loc.path.pop().is_some()
        {
            u.open(loc);
        }
    });
    let u = ui.clone();
    ui.reload.connect_clicked(move |_| {
        let loc = u.loc.borrow().clone();
        if let Some(loc) = loc {
            u.open(loc);
        }
        if let Some(d) = u.discovery.borrow().as_ref() {
            d.refresh();
        }
    });
    let u = ui.clone();
    ui.dl_here.connect_clicked(move |_| {
        let loc = u.loc.borrow().clone();
        if let Some(loc) = loc {
            let path = loc.path.clone();
            u.start_download(&loc, path, true);
        }
    });

    // Folders dropped from the file manager.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |_, value, _, _| {
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        u.add_shares(paths);
        true
    });
    ui.window.add_controller(drop);

    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            u.handle(ev);
        }
    });

    ui.refresh_my_list();
    ui.show_location();
    // Not the name field: typing would rename this computer by accident.
    GtkWindowExt::set_focus(&ui.window, Some(&add));
    Ok(ui)
}

impl Ui {
    fn save(&self) {
        if let Err(e) = self.cfg.borrow().save(&self.opts.config_dir) {
            eprintln!("sls: saving settings: {e:#}");
        }
    }

    fn set_background(&self, on: bool) {
        self.cfg.borrow_mut().run_in_background = on;
        self.save();
        self.window.set_hide_on_close(on);
        let dir = self
            .opts
            .custom_config_dir
            .then_some(self.opts.config_dir.as_path());
        if let Err(e) = autostart::set_enabled(&autostart::default_path(), on, &exec_path(), dir) {
            eprintln!("sls: autostart: {e}");
        }
    }

    // ---- my shares

    fn choose_folders(self: &Rc<Self>) {
        let dlg = gtk::FileChooserNative::new(
            Some("Share folders"),
            Some(&self.window),
            gtk::FileChooserAction::SelectFolder,
            Some("_Share"),
            Some("_Cancel"),
        );
        dlg.set_modal(true);
        dlg.set_select_multiple(true);
        let u = self.clone();
        dlg.connect_response(move |d, resp| {
            if resp == gtk::ResponseType::Accept {
                let files = d.files();
                let paths: Vec<PathBuf> = (0..files.n_items())
                    .filter_map(|i| files.item(i))
                    .filter_map(|o| o.downcast::<gio::File>().ok())
                    .filter_map(|f| f.path())
                    .collect();
                u.add_shares(paths);
            }
            u.chooser.borrow_mut().take();
        });
        dlg.show();
        *self.chooser.borrow_mut() = Some(dlg);
    }

    fn add_shares(self: &Rc<Self>, paths: Vec<PathBuf>) {
        {
            let mut cfg = self.cfg.borrow_mut();
            for p in paths {
                let p = p.canonicalize().unwrap_or(p);
                if p.is_dir() && !cfg.shares.contains(&p) {
                    cfg.shares.push(p);
                }
            }
        }
        self.shares_changed();
    }

    fn shares_changed(self: &Rc<Self>) {
        self.save();
        self.state
            .set_shares(shares::from_paths(&self.cfg.borrow().shares));
        self.refresh_my_list();
    }

    fn refresh_my_list(self: &Rc<Self>) {
        clear(&self.my_list);
        for (i, share) in self.state.shares().into_iter().enumerate() {
            let row = hbox(10);
            margins(&row, 6);
            row.append(&gtk::Image::from_icon_name("folder-symbolic"));
            let text = vbox(0);
            text.set_hexpand(true);
            let name = markup_label(&format!("<b>{}</b>", glib::markup_escape_text(&share.name)));
            name.set_ellipsize(pango::EllipsizeMode::End);
            text.append(&name);
            let missing = if share.path.is_dir() {
                ""
            } else {
                "  (missing)"
            };
            text.append(&dim(&format!("{}{missing}", share.path.display())));
            row.append(&text);
            let open = icon_button("folder-open-symbolic", "Open in Files");
            let path = share.path.clone();
            open.connect_clicked(move |_| open_external(&path.display().to_string()));
            row.append(&open);
            let remove = icon_button("list-remove-symbolic", "Stop sharing this folder");
            let u = self.clone();
            remove.connect_clicked(move |_| {
                {
                    let mut cfg = u.cfg.borrow_mut();
                    if i < cfg.shares.len() {
                        cfg.shares.remove(i);
                    }
                }
                u.shares_changed();
            });
            row.append(&remove);
            self.my_list.append(&row);
        }
    }

    // ---- the network

    fn handle(self: &Rc<Self>, ev: Ev) {
        match ev {
            Ev::Peers(peers) => self.peers_changed(peers),
            Ev::Listing { seq, result } => {
                if seq == self.seq.get() {
                    self.fill_entries(result);
                }
            }
            Ev::DlProgress { id, progress } => self.download_progress(id, &progress),
            Ev::DlDone { id, result } => self.download_done(id, result),
        }
    }

    fn peers_changed(self: &Rc<Self>, peers: Vec<Peer>) {
        clear(&self.net_list);
        let mut rows = Vec::new();
        let loc = self.loc.borrow().clone();
        let mut selected = None;
        for p in &peers {
            let head = hbox(8);
            margins(&head, 6);
            head.append(&gtk::Image::from_icon_name("computer-symbolic"));
            let name = markup_label(&format!("<b>{}</b>", glib::markup_escape_text(&p.name)));
            name.set_ellipsize(pango::EllipsizeMode::End);
            head.append(&name);
            let addr = dim(&p.addr.ip().to_string());
            addr.set_hexpand(true);
            addr.set_xalign(1.0);
            head.append(&addr);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&head));
            row.set_activatable(false);
            row.set_selectable(false);
            self.net_list.append(&row);
            rows.push(None);
            if p.shares.is_empty() {
                let none = dim("shares nothing at the moment");
                none.set_margin_start(36);
                none.set_margin_bottom(6);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&none));
                row.set_activatable(false);
                row.set_selectable(false);
                self.net_list.append(&row);
                rows.push(None);
            }
            for s in &p.shares {
                let line = hbox(8);
                margins(&line, 4);
                line.set_margin_start(28);
                line.append(&gtk::Image::from_icon_name("folder-remote-symbolic"));
                let l = gtk::Label::new(Some(s));
                l.set_xalign(0.0);
                l.set_ellipsize(pango::EllipsizeMode::End);
                line.append(&l);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&line));
                self.net_list.append(&row);
                if loc
                    .as_ref()
                    .is_some_and(|l| l.peer_id == p.id && &l.share == s)
                {
                    selected = Some(row.clone());
                }
                rows.push(Some((p.id.clone(), s.clone())));
            }
        }
        if let Some(row) = selected {
            self.net_list.select_row(Some(&row));
        }
        *self.net_rows.borrow_mut() = rows;

        // The browsed peer may have moved, stopped sharing, or left.
        if let Some(mut l) = loc {
            match peers.iter().find(|p| p.id == l.peer_id) {
                Some(p) if p.shares.contains(&l.share) => {
                    if p.addr != l.addr || p.name != l.peer_name {
                        l.addr = p.addr;
                        l.peer_name = p.name.clone();
                        *self.loc.borrow_mut() = Some(l);
                        self.show_location();
                    }
                }
                Some(p) => {
                    let name = p.name.clone();
                    self.lost_location(&format!("{name} no longer shares “{}”.", l.share));
                }
                None => {
                    self.lost_location(&format!("{} is not on the network any more.", l.peer_name))
                }
            }
        }
        *self.peers.borrow_mut() = peers;
    }

    fn lost_location(&self, msg: &str) {
        *self.loc.borrow_mut() = None;
        self.seq.set(self.seq.get() + 1);
        clear(&self.entries_list);
        self.entries.borrow_mut().clear();
        self.entries_placeholder.set_text(msg);
        self.show_location();
    }

    fn show_location(&self) {
        let loc = self.loc.borrow();
        match loc.as_ref() {
            Some(l) => {
                let mut parts = vec![l.peer_name.clone(), l.share.clone()];
                parts.extend(l.path.iter().cloned());
                self.crumb.set_text(&parts.join("  ›  "));
                self.up.set_sensitive(!l.path.is_empty());
                self.dl_here.set_sensitive(true);
            }
            None => {
                self.crumb.set_text("");
                self.up.set_sensitive(false);
                self.dl_here.set_sensitive(false);
            }
        }
        self.reload.set_sensitive(true);
    }

    fn open(self: &Rc<Self>, loc: Location) {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        *self.loc.borrow_mut() = Some(loc.clone());
        self.show_location();
        clear(&self.entries_list);
        self.entries.borrow_mut().clear();
        self.entries_placeholder.set_text("Loading…");
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result =
                client::list(loc.addr, &loc.share, &loc.path).map_err(|e| format!("{e:#}"));
            let _ = tx.send_blocking(Ev::Listing { seq, result });
        });
    }

    fn fill_entries(self: &Rc<Self>, result: Result<Vec<Entry>, String>) {
        clear(&self.entries_list);
        let entries = match result {
            Ok(e) => e,
            Err(e) => {
                self.entries_placeholder
                    .set_text(&format!("Cannot open this folder:\n{e}"));
                self.entries.borrow_mut().clear();
                return;
            }
        };
        self.entries_placeholder.set_text("This folder is empty.");
        for e in &entries {
            let row = hbox(10);
            margins(&row, 4);
            row.set_margin_start(10);
            row.append(&gtk::Image::from_icon_name(if e.dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            let name = gtk::Label::new(Some(&e.name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(pango::EllipsizeMode::Middle);
            row.append(&name);
            if !e.dir {
                row.append(&dim(&human_size(e.size)));
            }
            let dl = icon_button(
                "folder-download-symbolic",
                if e.dir {
                    "Download this folder"
                } else {
                    "Download"
                },
            );
            let u = self.clone();
            let entry = e.clone();
            dl.connect_clicked(move |_| u.entry_chosen(&entry, true));
            row.append(&dl);
            self.entries_list.append(&row);
        }
        *self.entries.borrow_mut() = entries;
    }

    /// Click on a row (folders open, files download) or its download button.
    fn entry_chosen(self: &Rc<Self>, e: &Entry, download: bool) {
        let Some(mut loc) = self.loc.borrow().clone() else {
            return;
        };
        if e.dir && !download {
            loc.path.push(e.name.clone());
            self.open(loc);
        } else {
            let mut path = loc.path.clone();
            path.push(e.name.clone());
            self.start_download(&loc, path, e.dir);
        }
    }

    // ---- downloads

    fn start_download(self: &Rc<Self>, loc: &Location, path: Vec<String>, is_dir: bool) {
        let id = self.next_dl.get();
        self.next_dl.set(id + 1);
        let name = path.last().cloned().unwrap_or_else(|| loc.share.clone());
        let cancel = Arc::new(AtomicBool::new(false));

        let widget = vbox(4);
        let top = hbox(6);
        let label = gtk::Label::new(Some(&format!("{name} — starting…")));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(pango::EllipsizeMode::Middle);
        let action = gtk::Button::with_label("Cancel");
        let dismiss = icon_button("window-close-symbolic", "Remove from the list");
        dismiss.set_visible(false);
        top.append(&label);
        top.append(&action);
        top.append(&dismiss);
        let bar = gtk::ProgressBar::new();
        widget.append(&top);
        widget.append(&bar);
        self.downloads_box.prepend(&widget);
        self.downloads_scroll.set_visible(true);

        let u = self.clone();
        action.connect_clicked(move |_| {
            let downloads = u.downloads.borrow();
            let Some(d) = downloads.get(&id) else { return };
            match &d.saved {
                Some(path) => {
                    // A folder opens itself; a file opens the folder it is in.
                    let target = if path.is_dir() {
                        path.clone()
                    } else {
                        path.parent().unwrap_or(path).to_path_buf()
                    };
                    open_external(&target.display().to_string());
                }
                None => d.cancel.store(true, Ordering::Relaxed),
            }
        });
        let u = self.clone();
        dismiss.connect_clicked(move |_| {
            if let Some(d) = u.downloads.borrow_mut().remove(&id) {
                u.downloads_box.remove(&d.widget);
            }
            u.downloads_scroll
                .set_visible(!u.downloads.borrow().is_empty());
        });
        self.downloads.borrow_mut().insert(
            id,
            DlRow {
                widget,
                label,
                bar,
                action,
                dismiss,
                cancel: cancel.clone(),
                name: name.clone(),
                saved: None,
            },
        );

        let tx = self.tx.clone();
        let addr = loc.addr;
        let share = loc.share.clone();
        let dest = self.cfg.borrow().download_dir();
        thread::spawn(move || {
            let ptx = tx.clone();
            let result = client::download(addr, &share, &path, is_dir, &dest, &cancel, &mut |p| {
                let _ = ptx.send_blocking(Ev::DlProgress {
                    id,
                    progress: p.clone(),
                });
            });
            let result = result.map_err(|e| {
                if client::is_cancelled(&e) {
                    "cancelled".into()
                } else {
                    format!("{e:#}")
                }
            });
            let _ = tx.send_blocking(Ev::DlDone { id, result });
        });
    }

    fn download_progress(&self, id: u64, p: &Progress) {
        let downloads = self.downloads.borrow();
        let Some(d) = downloads.get(&id) else { return };
        if d.saved.is_some() {
            return;
        }
        let mut text = format!(
            "{} — {} of {}",
            d.name,
            human_size(p.done),
            human_size(p.total)
        );
        if p.files_total > 1 {
            text.push_str(&format!(" · {} of {} files", p.files_done, p.files_total));
        }
        d.label.set_text(&text);
        d.bar.set_fraction(if p.total > 0 {
            p.done as f64 / p.total as f64
        } else {
            0.0
        });
    }

    fn download_done(&self, id: u64, result: Result<PathBuf, String>) {
        let mut downloads = self.downloads.borrow_mut();
        let Some(d) = downloads.get_mut(&id) else {
            return;
        };
        d.dismiss.set_visible(true);
        match result {
            Ok(path) => {
                d.label
                    .set_text(&format!("✓ {} — saved to {}", d.name, path.display()));
                d.label.set_tooltip_text(Some(&path.display().to_string()));
                d.bar.set_fraction(1.0);
                d.action.set_label("Show");
                d.saved = Some(path);
            }
            Err(e) => {
                d.label.set_text(&format!("✗ {} — {e}", d.name));
                d.label.set_tooltip_text(Some(&e));
                d.action.set_visible(false);
            }
        }
    }
}
