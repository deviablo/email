// SPDX-License-Identifier: GPL-3.0-or-later

//! The small icon in the panel while E-Mail runs in the background, over
//! the StatusNotifierItem protocol (KDE, and Ubuntu's AppIndicator
//! extension) with its menu over DBusMenu. Built on GIO's D-Bus support, so
//! no further library is needed.
//!
//! The icon lives on a D-Bus connection of its own: closing that connection
//! is how it disappears again, as the protocol has no "unregister". Without
//! a panel that shows such icons (GNOME without the extension) nothing is
//! shown; the icon appears as soon as a panel starts.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gettextrs::gettext;
use gtk::glib::prelude::*;
use gtk::{gio, glib};

use crate::config::APP_ID;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";

const ITEM_XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="IconPixmap" type="a(iiay)" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
    <signal name="NewIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus"><arg type="s"/></signal>
  </interface>
</node>"#;

const MENU_XML: &str = r#"<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/>
      <arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" direction="in"/><arg type="as" direction="in"/>
      <arg type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" direction="in"/><arg type="s" direction="in"/>
      <arg type="v" direction="in"/><arg type="u" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg type="a(isvu)" direction="in"/><arg type="ai" direction="out"/>
    </method>
    <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
    <method name="AboutToShowGroup">
      <arg type="ai" direction="in"/><arg type="ai" direction="out"/><arg type="ai" direction="out"/>
    </method>
    <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
    <signal name="ItemsPropertiesUpdated">
      <arg type="a(ia{sv})"/><arg type="a(ias)"/>
    </signal>
  </interface>
</node>"#;

/// The entries of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Open = 1,
    Compose = 2,
    Quit = 4,
}

impl MenuEntry {
    fn from_id(id: i32) -> Option<MenuEntry> {
        match id {
            1 => Some(MenuEntry::Open),
            2 => Some(MenuEntry::Compose),
            4 => Some(MenuEntry::Quit),
            _ => None,
        }
    }
}

/// The icon shows while the app keeps running in the background with its
/// window closed, and only if the user wants it.
pub fn visible(
    run_in_background: bool,
    show_icon: bool,
    has_accounts: bool,
    window_visible: bool,
) -> bool {
    run_in_background && show_icon && has_accounts && !window_visible
}

/// The icon name: with a dot while any inbox has unread mail.
pub fn icon_name(unread: u32) -> String {
    if unread > 0 {
        format!("{APP_ID}-unread-symbolic")
    } else {
        format!("{APP_ID}-symbolic")
    }
}

/// "E-Mail" or "E-Mail – 3 unread".
pub fn tooltip(unread: u32) -> String {
    if unread == 0 {
        "E-Mail".to_owned()
    } else {
        crate::widgets::fmt(&gettext("E-Mail – {} unread"), &[&unread.to_string()])
    }
}

type Callback = Box<dyn Fn(MenuEntry)>;

pub struct Tray {
    wanted: Cell<bool>,
    unread: Cell<u32>,
    watcher: Cell<bool>,
    connecting: Cell<bool>,
    connection: RefCell<Option<gio::DBusConnection>>,
    on_entry: Callback,
    weak: RefCell<Weak<Tray>>,
    _watch: RefCell<Option<gio::WatcherId>>,
}

impl Tray {
    pub fn new(on_entry: impl Fn(MenuEntry) + 'static) -> Rc<Tray> {
        let this = Rc::new(Tray {
            wanted: Cell::new(false),
            unread: Cell::new(0),
            watcher: Cell::new(false),
            connecting: Cell::new(false),
            connection: RefCell::default(),
            on_entry: Box::new(on_entry),
            weak: RefCell::default(),
            _watch: RefCell::default(),
        });
        *this.weak.borrow_mut() = Rc::downgrade(&this);
        let appeared = Rc::downgrade(&this);
        let vanished = Rc::downgrade(&this);
        let watch = gio::bus_watch_name(
            gio::BusType::Session,
            WATCHER,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(this) = appeared.upgrade() {
                    this.watcher.set(true);
                    // A panel that restarted forgets earlier icons.
                    this.hide();
                    this.refresh();
                }
            },
            move |_, _| {
                if let Some(this) = vanished.upgrade() {
                    this.watcher.set(false);
                    this.hide();
                }
            },
        );
        *this._watch.borrow_mut() = Some(watch);
        this
    }

    pub fn set_wanted(&self, wanted: bool) {
        if self.wanted.replace(wanted) != wanted {
            self.refresh();
        }
    }

    pub fn set_unread(&self, unread: u32) {
        if self.unread.replace(unread) == unread {
            return;
        }
        if let Some(connection) = self.connection.borrow().as_ref() {
            for signal in ["NewIcon", "NewToolTip"] {
                let _ = connection.emit_signal(
                    None,
                    ITEM_PATH,
                    "org.kde.StatusNotifierItem",
                    signal,
                    None,
                );
            }
        }
    }

    fn refresh(&self) {
        let show = self.wanted.get() && self.watcher.get();
        if !show {
            self.hide();
            return;
        }
        if self.connection.borrow().is_some() || self.connecting.replace(true) {
            return;
        }
        let weak = self.weak.borrow().clone();
        glib::spawn_future_local(async move {
            let result = register(weak.clone()).await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.connecting.set(false);
            match result {
                Ok(connection) => {
                    *this.connection.borrow_mut() = Some(connection);
                    // Things may have changed while connecting.
                    if !(this.wanted.get() && this.watcher.get()) {
                        this.hide();
                    }
                }
                Err(error) => tracing::info!("no icon in the panel: {error}"),
            }
        });
    }

    fn hide(&self) {
        if let Some(connection) = self.connection.take() {
            glib::spawn_future_local(async move {
                let _ = connection.close_future().await;
            });
        }
    }

    fn item_property(&self, name: &str) -> glib::Variant {
        let unread = self.unread.get();
        match name {
            "Category" => "Communications".to_variant(),
            "Id" => APP_ID.to_variant(),
            "Title" => "E-Mail".to_variant(),
            "Status" => "Active".to_variant(),
            "WindowId" => 0i32.to_variant(),
            "IconName" => icon_name(unread).to_variant(),
            "IconThemePath" | "OverlayIconName" | "AttentionIconName" => "".to_variant(),
            "IconPixmap" => Vec::<(i32, i32, Vec<u8>)>::new().to_variant(),
            "ToolTip" => (
                icon_name(unread),
                Vec::<(i32, i32, Vec<u8>)>::new(),
                "E-Mail".to_owned(),
                tooltip(unread),
            )
                .to_variant(),
            "ItemIsMenu" => false.to_variant(),
            "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH.to_owned())
                .map(|path| path.to_variant())
                .unwrap_or_else(|_| "".to_variant()),
            _ => "".to_variant(),
        }
    }

    fn entry(&self, entry: MenuEntry) {
        let weak = self.weak.borrow().clone();
        // Not from inside the D-Bus call.
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                (this.on_entry)(entry);
            }
        });
    }
}

/// Opens a connection of its own, puts the icon and its menu on it and
/// tells the panel.
async fn register(weak: Weak<Tray>) -> Result<gio::DBusConnection, glib::Error> {
    let address =
        gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let connection = gio::DBusConnection::for_address_future(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
    )
    .await?;

    let item = gio::DBusNodeInfo::for_xml(ITEM_XML)?;
    let item_weak = weak.clone();
    let property_weak = weak.clone();
    connection
        .register_object(ITEM_PATH, &item.interfaces()[0])
        .method_call(move |_, _, _, _, method, _, invocation| {
            if let Some(this) = item_weak.upgrade()
                && matches!(method, "Activate" | "SecondaryActivate")
            {
                this.entry(MenuEntry::Open);
            }
            invocation.return_value(None);
        })
        .property(move |_, _, _, _, name| {
            property_weak
                .upgrade()
                .map_or_else(|| "".to_variant(), |this| this.item_property(name))
        })
        .build()?;

    let menu = gio::DBusNodeInfo::for_xml(MENU_XML)?;
    let menu_weak = weak.clone();
    connection
        .register_object(MENU_PATH, &menu.interfaces()[0])
        .method_call(move |_, _, _, _, method, parameters, invocation| {
            let reply = menu_call(&menu_weak, method, &parameters);
            invocation.return_value(reply.as_ref());
        })
        .property(|_, _, _, _, name| match name {
            "Version" => 3u32.to_variant(),
            "TextDirection" => "ltr".to_variant(),
            "Status" => "normal".to_variant(),
            _ => Vec::<String>::new().to_variant(),
        })
        .build()?;

    let unique = connection.unique_name().map(|name| name.to_string());
    tracing::debug!("panel icon on {unique:?}");
    connection
        .call_future(
            Some(WATCHER),
            "/StatusNotifierWatcher",
            WATCHER,
            "RegisterStatusNotifierItem",
            Some(&(ITEM_PATH,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await?;
    Ok(connection)
}

fn menu_item(id: i32, properties: Vec<(&str, glib::Variant)>) -> glib::Variant {
    let properties: HashMap<String, glib::Variant> = properties
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    (id, properties, Vec::<glib::Variant>::new()).to_variant()
}

fn menu_items() -> Vec<glib::Variant> {
    vec![
        menu_item(
            MenuEntry::Open as i32,
            vec![("label", gettext("Open E-Mail").to_variant())],
        ),
        menu_item(
            MenuEntry::Compose as i32,
            vec![("label", gettext("New Mail").to_variant())],
        ),
        menu_item(3, vec![("type", "separator".to_variant())]),
        menu_item(
            MenuEntry::Quit as i32,
            vec![("label", gettext("Quit").to_variant())],
        ),
    ]
}

fn menu_layout() -> glib::Variant {
    let mut root: HashMap<String, glib::Variant> = HashMap::new();
    root.insert("children-display".to_owned(), "submenu".to_variant());
    (0i32, root, menu_items()).to_variant()
}

fn layout_reply() -> glib::Variant {
    glib::Variant::tuple_from_iter([1u32.to_variant(), menu_layout()])
}

fn menu_call(weak: &Weak<Tray>, method: &str, parameters: &glib::Variant) -> Option<glib::Variant> {
    match method {
        "GetLayout" => Some(layout_reply()),
        "GetGroupProperties" => {
            let wanted: Vec<i32> = parameters
                .child_value(0)
                .get::<Vec<i32>>()
                .unwrap_or_default();
            let items: Vec<(i32, HashMap<String, glib::Variant>)> = menu_items()
                .iter()
                .filter_map(|item| {
                    let id = item.child_value(0).get::<i32>()?;
                    let properties = item
                        .child_value(1)
                        .get::<HashMap<String, glib::Variant>>()?;
                    (wanted.is_empty() || wanted.contains(&id)).then_some((id, properties))
                })
                .collect();
            Some((items,).to_variant())
        }
        "GetProperty" => Some(("".to_variant(),).to_variant()),
        "Event" => {
            let id = parameters.child_value(0).get::<i32>().unwrap_or_default();
            let event = parameters
                .child_value(1)
                .get::<String>()
                .unwrap_or_default();
            if event == "clicked"
                && let (Some(this), Some(entry)) = (weak.upgrade(), MenuEntry::from_id(id))
            {
                this.entry(entry);
            }
            None
        }
        "EventGroup" => {
            if let Some(events) = parameters
                .child_value(0)
                .get::<Vec<(i32, String, glib::Variant, u32)>>()
            {
                for (id, event, ..) in events {
                    if event == "clicked"
                        && let (Some(this), Some(entry)) = (weak.upgrade(), MenuEntry::from_id(id))
                    {
                        this.entry(entry);
                    }
                }
            }
            Some((Vec::<i32>::new(),).to_variant())
        }
        "AboutToShow" => Some((false,).to_variant()),
        "AboutToShowGroup" => Some((Vec::<i32>::new(), Vec::<i32>::new()).to_variant()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_only_in_the_background() {
        assert!(visible(true, true, true, false));
        assert!(!visible(true, true, true, true), "window open");
        assert!(!visible(false, true, true, false), "no background mode");
        assert!(!visible(true, false, true, false), "switched off");
        assert!(!visible(true, true, false, false), "no account yet");
    }

    #[test]
    fn icon_shows_unread_mail() {
        assert_eq!(icon_name(0), "io.github.deviablo.EMail-symbolic");
        assert_eq!(icon_name(3), "io.github.deviablo.EMail-unread-symbolic");
        assert_eq!(tooltip(0), "E-Mail");
    }

    #[test]
    fn menu_has_the_right_shape() {
        let layout = layout_reply();
        assert_eq!(layout.type_().as_str(), "(u(ia{sv}av))");
        assert_eq!(menu_items().len(), 4);
        assert_eq!(MenuEntry::from_id(4), Some(MenuEntry::Quit));
        assert_eq!(MenuEntry::from_id(3), None);
    }
}
