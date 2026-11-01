//! The icon in the top bar ("system tray"), for opening the window of an
//! instance running in the background.
//!
//! GTK 4 has no tray API.  Ubuntu shows tray icons with its AppIndicator
//! extension, which implements the freedesktop StatusNotifierItem protocol:
//! the app exports two D-Bus objects and registers with the
//! `org.kde.StatusNotifierWatcher`.  This module does that with GIO directly.
//!
//! * `/StatusNotifierItem` (`org.kde.StatusNotifierItem`): icon, tooltip,
//!   and `Activate` (left click)
//! * `/MenuBar` (`com.canonical.dbusmenu`): the menu ("Open sls", "Quit")
//!
//! While file offers wait for an answer the status is `NeedsAttention`: the
//! icon gets a badge and the menu and tooltip say how many.

use std::cell::Cell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, Variant, VariantDict, VariantTy, prelude::*};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

const XML: &str = r#"<node>
<interface name="org.kde.StatusNotifierItem">
  <property name="Category" type="s" access="read"/>
  <property name="Id" type="s" access="read"/>
  <property name="Title" type="s" access="read"/>
  <property name="Status" type="s" access="read"/>
  <property name="IconName" type="s" access="read"/>
  <property name="AttentionIconName" type="s" access="read"/>
  <property name="IconThemePath" type="s" access="read"/>
  <property name="ItemIsMenu" type="b" access="read"/>
  <property name="Menu" type="o" access="read"/>
  <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
  <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
  <signal name="NewTitle"/>
  <signal name="NewIcon"/>
  <signal name="NewAttentionIcon"/>
  <signal name="NewToolTip"/>
  <signal name="NewStatus"><arg name="status" type="s"/></signal>
</interface>
<interface name="com.canonical.dbusmenu">
  <property name="Version" type="u" access="read"/>
  <property name="TextDirection" type="s" access="read"/>
  <property name="Status" type="s" access="read"/>
  <property name="IconThemePath" type="as" access="read"/>
  <method name="GetLayout">
    <arg name="parentId" type="i" direction="in"/>
    <arg name="recursionDepth" type="i" direction="in"/>
    <arg name="propertyNames" type="as" direction="in"/>
    <arg name="revision" type="u" direction="out"/>
    <arg name="layout" type="(ia{sv}av)" direction="out"/>
  </method>
  <method name="GetGroupProperties">
    <arg name="ids" type="ai" direction="in"/>
    <arg name="propertyNames" type="as" direction="in"/>
    <arg name="properties" type="a(ia{sv})" direction="out"/>
  </method>
  <method name="GetProperty">
    <arg name="id" type="i" direction="in"/>
    <arg name="name" type="s" direction="in"/>
    <arg name="value" type="v" direction="out"/>
  </method>
  <method name="Event">
    <arg name="id" type="i" direction="in"/>
    <arg name="eventId" type="s" direction="in"/>
    <arg name="data" type="v" direction="in"/>
    <arg name="timestamp" type="u" direction="in"/>
  </method>
  <method name="EventGroup">
    <arg name="events" type="a(isvu)" direction="in"/>
    <arg name="idErrors" type="ai" direction="out"/>
  </method>
  <method name="AboutToShow">
    <arg name="id" type="i" direction="in"/>
    <arg name="needUpdate" type="b" direction="out"/>
  </method>
  <method name="AboutToShowGroup">
    <arg name="ids" type="ai" direction="in"/>
    <arg name="updatesNeeded" type="ai" direction="out"/>
    <arg name="idErrors" type="ai" direction="out"/>
  </method>
  <signal name="ItemsPropertiesUpdated">
    <arg name="updatedProps" type="a(ia{sv})"/>
    <arg name="removedProps" type="a(ias)"/>
  </signal>
  <signal name="LayoutUpdated">
    <arg name="revision" type="u"/>
    <arg name="parent" type="i"/>
  </signal>
</interface>
</node>"#;

/// Menu item ids.
const ROOT: i32 = 0;
const OPEN: i32 = 1;
const SEPARATOR: i32 = 2;
const QUIT: i32 = 3;

/// What the icon shows; shared by the D-Bus handlers.
struct Shown {
    /// Offers waiting for an answer.
    waiting: Cell<usize>,
    revision: Cell<u32>,
    icon: String,
    attention_icon: String,
}

impl Shown {
    fn status(&self) -> &'static str {
        if self.waiting.get() > 0 {
            "NeedsAttention"
        } else {
            "Active"
        }
    }

    fn waiting_text(&self) -> Option<String> {
        match self.waiting.get() {
            0 => None,
            1 => Some("Someone wants to send you a file".into()),
            n => Some(format!("{n} file offers are waiting for your answer")),
        }
    }

    fn tooltip(&self) -> Variant {
        let text = self
            .waiting_text()
            .unwrap_or_else(|| "Sharing folders on the local network".into());
        let pixmaps = Variant::array_from_iter_with_type(
            VariantTy::new("(iiay)").unwrap(),
            std::iter::empty::<Variant>(),
        );
        Variant::tuple_from_iter([
            self.icon.to_variant(),
            pixmaps,
            "sls".to_variant(),
            text.to_variant(),
        ])
    }

    fn open_label(&self) -> String {
        match self.waiting.get() {
            0 => "Open sls".into(),
            1 => "Open sls (1 file offered)".into(),
            n => format!("Open sls ({n} files offered)"),
        }
    }

    fn props(&self, id: i32) -> Vec<(&'static str, Variant)> {
        match id {
            ROOT => vec![("children-display", "submenu".to_variant())],
            OPEN => vec![("label", self.open_label().to_variant())],
            SEPARATOR => vec![("type", "separator".to_variant())],
            QUIT => vec![("label", "Quit".to_variant())],
            _ => vec![],
        }
    }

    /// `(ia{sv}av)` for `id` and its children.
    fn layout(&self, id: i32) -> Variant {
        let children: Vec<Variant> = if id == ROOT {
            [OPEN, SEPARATOR, QUIT]
                .into_iter()
                .map(|c| Variant::from_variant(&self.layout(c)))
                .collect()
        } else {
            vec![]
        };
        Variant::tuple_from_iter([
            id.to_variant(),
            dict(&self.props(id)),
            Variant::array_from_iter_with_type(VariantTy::VARIANT, children),
        ])
    }
}

fn dict(props: &[(&str, Variant)]) -> Variant {
    let d = VariantDict::new(None);
    for (k, v) in props {
        d.insert_value(k, v);
    }
    d.end()
}

pub struct Tray {
    conn: gio::DBusConnection,
    shown: Rc<Shown>,
    registrations: Vec<gio::RegistrationId>,
    /// Stops watching for the StatusNotifierWatcher.
    unwatch: Option<Box<dyn FnOnce()>>,
}

impl Tray {
    /// Exports the icon on `conn` (the app's session bus connection).
    /// `icon` is an icon theme name.  Clicks call `open`; the menu also
    /// offers `quit`.
    pub fn start(
        conn: &gio::DBusConnection,
        icon: &str,
        attention_icon: &str,
        open: impl Fn() + 'static,
        quit: impl Fn() + 'static,
    ) -> Result<Tray, glib::Error> {
        let node = gio::DBusNodeInfo::for_xml(XML)?;
        let item_iface = node.lookup_interface(ITEM_IFACE).unwrap();
        let menu_iface = node.lookup_interface(MENU_IFACE).unwrap();
        let shown = Rc::new(Shown {
            waiting: Cell::new(0),
            revision: Cell::new(1),
            icon: icon.to_string(),
            attention_icon: attention_icon.to_string(),
        });
        let open: Rc<dyn Fn()> = Rc::new(open);
        let quit: Rc<dyn Fn()> = Rc::new(quit);

        let s = shown.clone();
        let o = open.clone();
        let item = conn
            .register_object(ITEM_PATH, &item_iface)
            .method_call(move |_, _, _, _, method, _, inv| {
                if matches!(method, "Activate" | "SecondaryActivate") {
                    o();
                }
                inv.return_value(None);
            })
            .property(move |_, _, _, _, prop| match prop {
                "Category" => "Communications".to_variant(),
                "Id" => "sls".to_variant(),
                "Title" => "sls".to_variant(),
                "Status" => s.status().to_variant(),
                "IconName" => s.icon.to_variant(),
                "AttentionIconName" => s.attention_icon.to_variant(),
                "IconThemePath" => "".to_variant(),
                "ItemIsMenu" => false.to_variant(),
                "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH)
                    .unwrap()
                    .to_variant(),
                "ToolTip" => s.tooltip(),
                _ => "".to_variant(),
            })
            .build()?;

        let s = shown.clone();
        let menu = conn
            .register_object(MENU_PATH, &menu_iface)
            .method_call(move |_, _, _, _, method, params, inv| {
                let reply = match method {
                    "GetLayout" => {
                        let parent = params.child_value(0).get::<i32>().unwrap_or(ROOT);
                        Some(Variant::tuple_from_iter([
                            s.revision.get().to_variant(),
                            s.layout(parent),
                        ]))
                    }
                    "GetGroupProperties" => {
                        let ids = params.child_value(0).get::<Vec<i32>>().unwrap_or_default();
                        let ids = if ids.is_empty() {
                            vec![ROOT, OPEN, SEPARATOR, QUIT]
                        } else {
                            ids
                        };
                        let items = ids.into_iter().map(|id| {
                            Variant::tuple_from_iter([id.to_variant(), dict(&s.props(id))])
                        });
                        Some(Variant::tuple_from_iter([
                            Variant::array_from_iter_with_type(
                                VariantTy::new("(ia{sv})").unwrap(),
                                items,
                            ),
                        ]))
                    }
                    "GetProperty" => {
                        let id = params.child_value(0).get::<i32>().unwrap_or(-1);
                        let name = params.child_value(1).get::<String>().unwrap_or_default();
                        let value = s
                            .props(id)
                            .into_iter()
                            .find(|(k, _)| *k == name)
                            .map_or_else(|| "".to_variant(), |(_, v)| v);
                        Some(Variant::tuple_from_iter([Variant::from_variant(&value)]))
                    }
                    "Event" => {
                        let id = params.child_value(0).get::<i32>().unwrap_or(-1);
                        let event = params.child_value(1).get::<String>().unwrap_or_default();
                        if event == "clicked" {
                            clicked(id, &open, &quit);
                        }
                        None
                    }
                    "EventGroup" => {
                        let events = params.child_value(0);
                        for e in events.iter() {
                            let id = e.child_value(0).get::<i32>().unwrap_or(-1);
                            if e.child_value(1).get::<String>().as_deref() == Some("clicked") {
                                clicked(id, &open, &quit);
                            }
                        }
                        Some((Vec::<i32>::new(),).to_variant())
                    }
                    "AboutToShow" => Some((false,).to_variant()),
                    "AboutToShowGroup" => Some((Vec::<i32>::new(), Vec::<i32>::new()).to_variant()),
                    _ => None,
                };
                inv.return_value(reply.as_ref());
            })
            .property(move |_, _, _, _, prop| match prop {
                "Version" => 3u32.to_variant(),
                "TextDirection" => "ltr".to_variant(),
                "Status" => "normal".to_variant(),
                "IconThemePath" => Vec::<String>::new().to_variant(),
                _ => "".to_variant(),
            })
            .build()?;

        // Register now and whenever the watcher (re)appears (GNOME Shell
        // restarts, the extension gets enabled later).
        let unique = conn.unique_name().map(|n| n.to_string());
        let unwatch = unique.map(|unique| {
            let id = gio::bus_watch_name_on_connection(
                conn,
                WATCHER,
                gio::BusNameWatcherFlags::NONE,
                move |conn, _, _| {
                    conn.call(
                        Some(WATCHER),
                        "/StatusNotifierWatcher",
                        WATCHER,
                        "RegisterStatusNotifierItem",
                        Some(&(unique.as_str(),).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        -1,
                        gio::Cancellable::NONE,
                        |r| {
                            if let Err(e) = r {
                                eprintln!("sls: tray icon: {e}");
                            }
                        },
                    );
                },
                |_, _| {},
            );
            Box::new(move || gio::bus_unwatch_name(id)) as Box<dyn FnOnce()>
        });

        Ok(Tray {
            conn: conn.clone(),
            shown,
            registrations: vec![item, menu],
            unwatch,
        })
    }

    /// How many offers wait for an answer (0: the normal icon).
    pub fn set_waiting(&self, n: usize) {
        if self.shown.waiting.replace(n) == n {
            return;
        }
        let s = &self.shown;
        s.revision.set(s.revision.get() + 1);
        let emit = |path: &str, iface: &str, signal: &str, args: Option<Variant>| {
            if let Err(e) = self
                .conn
                .emit_signal(None, path, iface, signal, args.as_ref())
            {
                eprintln!("sls: tray icon: {e}");
            }
        };
        emit(
            ITEM_PATH,
            ITEM_IFACE,
            "NewStatus",
            Some((s.status(),).to_variant()),
        );
        emit(ITEM_PATH, ITEM_IFACE, "NewToolTip", None);
        emit(
            MENU_PATH,
            MENU_IFACE,
            "LayoutUpdated",
            Some((s.revision.get(), ROOT).to_variant()),
        );
    }
}

fn clicked(id: i32, open: &Rc<dyn Fn()>, quit: &Rc<dyn Fn()>) {
    // Not from inside the D-Bus handler: `quit` ends the main loop.
    let f = match id {
        OPEN => open.clone(),
        QUIT => quit.clone(),
        _ => return,
    };
    glib::idle_add_local_once(move || f());
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(unwatch) = self.unwatch.take() {
            unwatch();
        }
        for r in self.registrations.drain(..) {
            let _ = self.conn.unregister_object(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(waiting: usize) -> Shown {
        Shown {
            waiting: Cell::new(waiting),
            revision: Cell::new(1),
            icon: "i".into(),
            attention_icon: "a".into(),
        }
    }

    #[test]
    fn menu_layout() {
        let s = shown(0);
        let layout = s.layout(ROOT);
        assert_eq!(layout.type_().as_str(), "(ia{sv}av)");
        assert_eq!(layout.child_value(2).n_children(), 3);
        let open = layout.child_value(2).child_value(0).as_variant().unwrap();
        let label = VariantDict::new(Some(&open.child_value(1)))
            .lookup::<String>("label")
            .unwrap();
        assert_eq!(label.as_deref(), Some("Open sls"));
        assert_eq!(s.status(), "Active");
        assert_eq!(s.tooltip().type_().as_str(), "(sa(iiay)ss)");
    }

    #[test]
    fn attention_while_offers_wait() {
        let s = shown(2);
        assert_eq!(s.status(), "NeedsAttention");
        assert_eq!(s.open_label(), "Open sls (2 files offered)");
        let text = s.tooltip().child_value(3).get::<String>().unwrap();
        assert!(text.contains("2 file offers"), "{text}");
    }
}
