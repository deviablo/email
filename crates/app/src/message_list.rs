// SPDX-License-Identifier: GPL-3.0-or-later

//! The message list: one row per conversation, newest first. A right click,
//! a long press or the menu key opens the actions of a row; rows can be
//! dragged onto a folder. In selection mode rows get check boxes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use crate::widgets::fmt;
use e_mail_core::model::{ThreadId, ThreadSummary};
use e_mail_core::search;
use gettextrs::gettext;
use gtk::prelude::*;
use gtk::{gio, glib};

/// What the menu of a row (or a shortcut) asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
    ToggleRead,
    ToggleStar,
    Delete,
}

/// What a dragged row carries: the conversations, comma separated.
pub const DRAG_PREFIX: &str = "e-mail-conversations:";

struct RowWidgets {
    root: gtk::Overlay,
    thread: Rc<Cell<ThreadId>>,
    avatar: adw::Avatar,
    check: gtk::CheckButton,
    unread_dot: gtk::Box,
    sender: gtk::Label,
    time: gtk::Label,
    subject: gtk::Label,
    count: gtk::Label,
    attachment: gtk::Image,
    star: gtk::Image,
    folder: gtk::Label,
}

/// How rows describe their senders: the own address shows as "Me", and
/// drafts say so instead of naming anyone.
#[derive(Clone, Debug, Default)]
pub struct ListContext {
    pub own_address: String,
    pub drafts: bool,
    /// Search words to highlight in the subject.
    pub highlight: Vec<String>,
    /// For search results: the folder each conversation was found in.
    pub folders: HashMap<ThreadId, String>,
}

type OpenCallback = Box<dyn Fn(ThreadId)>;
/// The conversation, the widget to point at and where.
type MenuCallback = Box<dyn Fn(ThreadId, gtk::Widget, f64, f64)>;
type SelectionCallback = Box<dyn Fn(usize)>;
type RequestCallback = Box<dyn Fn()>;

pub struct MessageList {
    pub widget: gtk::ScrolledWindow,
    model: gio::ListStore,
    selection: gtk::SingleSelection,
    updating: Cell<bool>,
    selecting: Cell<bool>,
    selected: RefCell<HashSet<ThreadId>>,
    context: RefCell<ListContext>,
    on_open: RefCell<Option<OpenCallback>>,
    on_open_window: RefCell<Option<OpenCallback>>,
    on_menu: RefCell<Option<MenuCallback>>,
    on_selection: RefCell<Option<SelectionCallback>>,
    on_request_selection: RefCell<Option<RequestCallback>>,
    /// Every row widget alive, to update the marks without rebinding.
    rows: RefCell<Vec<Weak<RowWidgets>>>,
    /// Where a Shift click range starts.
    anchor: Cell<Option<ThreadId>>,
}

impl MessageList {
    pub fn new() -> Rc<MessageList> {
        let model = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::builder()
            .model(&model)
            .autoselect(false)
            .can_unselect(true)
            .build();

        let this = Rc::new_cyclic(|weak: &Weak<MessageList>| {
            let factory = gtk::SignalListItemFactory::new();
            let setup_weak = weak.clone();
            factory.connect_setup(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (row, widgets) = build_row(&setup_weak);
                item.set_child(Some(&row));
                let widgets = Rc::new(widgets);
                if let Some(list) = setup_weak.upgrade() {
                    let mut rows = list.rows.borrow_mut();
                    rows.retain(|row| row.strong_count() > 0);
                    rows.push(Rc::downgrade(&widgets));
                }
                // SAFETY: "row" is only ever stored and read as `Rc<RowWidgets>`.
                unsafe { item.set_data("row", widgets) };
            });
            let bind_weak = weak.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(object) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                // SAFETY: see `connect_setup`.
                let Some(widgets) = (unsafe { item.data::<Rc<RowWidgets>>("row") }) else {
                    return;
                };
                let widgets = unsafe { widgets.as_ref() };
                if let Some(this) = bind_weak.upgrade() {
                    this.bind_row(widgets, &object.borrow::<ThreadSummary>());
                }
            });

            let view = gtk::ListView::builder()
                .model(&selection)
                .factory(&factory)
                .css_classes(["navigation-sidebar", "message-list"])
                .build();
            view.update_property(&[gtk::accessible::Property::Label(&gettext("Messages"))]);
            let keys = gtk::EventControllerKey::new();
            let key_weak = weak.clone();
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                let shift_f10 = key == gtk::gdk::Key::F10
                    && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                if (key == gtk::gdk::Key::Menu || shift_f10)
                    && let Some(list) = key_weak.upgrade()
                    && let Some((thread, row)) = list.focused_thread()
                {
                    let y = f64::from(row.height()) / 2.0;
                    list.menu(thread, &row, 48.0, y);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            view.add_controller(keys);
            let widget = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&view)
                .build();
            MessageList {
                widget,
                model,
                selection: selection.clone(),
                updating: Cell::new(false),
                selecting: Cell::new(false),
                selected: RefCell::default(),
                context: RefCell::default(),
                on_open: RefCell::default(),
                on_open_window: RefCell::default(),
                on_menu: RefCell::default(),
                on_selection: RefCell::default(),
                on_request_selection: RefCell::default(),
                rows: RefCell::default(),
                anchor: Cell::new(None),
            }
        });

        let weak = Rc::downgrade(&this);
        this.selection
            .connect_selected_item_notify(move |selection| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this.updating.get() {
                    return;
                }
                let Some(object) = selection
                    .selected_item()
                    .and_downcast::<glib::BoxedAnyObject>()
                else {
                    return;
                };
                let id = object.borrow::<ThreadSummary>().id;
                if this.selecting.get() {
                    this.toggle_selected(id);
                    this.updating.set(true);
                    selection.set_selected(gtk::INVALID_LIST_POSITION);
                    this.updating.set(false);
                    return;
                }
                if let Some(callback) = this.on_open.borrow().as_ref() {
                    callback(id);
                }
            });
        this
    }

    pub fn connect_open(&self, callback: impl Fn(ThreadId) + 'static) {
        *self.on_open.borrow_mut() = Some(Box::new(callback));
    }

    /// Called when a row is double-clicked, to open it in a window of its own.
    pub fn connect_open_window(&self, callback: impl Fn(ThreadId) + 'static) {
        *self.on_open_window.borrow_mut() = Some(Box::new(callback));
    }

    fn open_window(&self, thread: ThreadId) {
        if self.selecting.get() {
            return;
        }
        if let Some(callback) = self.on_open_window.borrow().as_ref() {
            callback(thread);
        }
    }

    /// Called with the number of selected conversations in selection mode.
    pub fn connect_selection_changed(&self, callback: impl Fn(usize) + 'static) {
        *self.on_selection.borrow_mut() = Some(Box::new(callback));
    }

    /// Asked for when a Ctrl or Shift click wants selection mode on.
    pub fn connect_request_selection(&self, callback: impl Fn() + 'static) {
        *self.on_request_selection.borrow_mut() = Some(Box::new(callback));
    }

    /// Ctrl click toggles one conversation, Shift click selects all from
    /// the last one picked (or the open one) to this one.
    fn modified_click(&self, thread: ThreadId, range: bool) {
        let open = self.open_thread();
        if !self.selecting.get() {
            if let Some(callback) = self.on_request_selection.borrow().as_ref() {
                callback();
            }
            if !self.selecting.get() {
                self.set_selection_mode(true);
            }
            if let Some(open) = open {
                self.selected.borrow_mut().insert(open);
                self.anchor.set(Some(open));
            }
        }
        let ids = self.thread_ids();
        let here = ids.iter().position(|id| *id == thread);
        let start = self
            .anchor
            .get()
            .and_then(|anchor| ids.iter().position(|id| *id == anchor));
        {
            let mut selected = self.selected.borrow_mut();
            match (range, here, start) {
                (true, Some(here), Some(start)) => {
                    let (from, to) = (start.min(here), start.max(here));
                    selected.extend(ids[from..=to].iter().copied());
                }
                _ => {
                    if !selected.remove(&thread) {
                        selected.insert(thread);
                    }
                    self.anchor.set(Some(thread));
                }
            }
        }
        self.refresh_marks();
        self.notify_selection();
    }

    fn thread_ids(&self) -> Vec<ThreadId> {
        (0..self.model.n_items())
            .filter_map(|index| {
                self.model
                    .item(index)
                    .and_downcast::<glib::BoxedAnyObject>()
            })
            .map(|object| object.borrow::<ThreadSummary>().id)
            .collect()
    }

    fn open_thread(&self) -> Option<ThreadId> {
        let position = self.selection.selected();
        if position == gtk::INVALID_LIST_POSITION {
            return None;
        }
        self.model
            .item(position)
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|object| object.borrow::<ThreadSummary>().id)
    }

    /// Updates check boxes and highlight of every visible row.
    fn refresh_marks(&self) {
        let selecting = self.selecting.get();
        let selected = self.selected.borrow();
        for row in self.rows.borrow().iter().filter_map(Weak::upgrade) {
            let picked = selecting && selected.contains(&row.thread.get());
            row.check.set_active(picked);
            if picked {
                row.root.add_css_class("picked");
            } else {
                row.root.remove_css_class("picked");
            }
        }
    }

    /// Asked for when the menu of a row should open.
    pub fn connect_menu(&self, callback: impl Fn(ThreadId, gtk::Widget, f64, f64) + 'static) {
        *self.on_menu.borrow_mut() = Some(Box::new(callback));
    }

    fn menu(&self, thread: ThreadId, widget: &gtk::Widget, x: f64, y: f64) {
        if let Some(callback) = self.on_menu.borrow().as_ref() {
            callback(thread, widget.clone(), x, y);
        }
    }

    /// The conversations a drag or menu on `thread` acts on: all selected
    /// ones in selection mode (if `thread` is among them), else `thread`.
    pub fn targets(&self, thread: ThreadId) -> Vec<ThreadId> {
        let selected = self.selected.borrow();
        if self.selecting.get() && selected.contains(&thread) {
            selected.iter().copied().collect()
        } else {
            vec![thread]
        }
    }

    /// The conversation of the row with the keyboard focus, if any.
    fn focused_thread(&self) -> Option<(ThreadId, gtk::Widget)> {
        let view = self.widget.child().and_downcast::<gtk::ListView>()?;
        let row = view.focus_child()?;
        let position = self.selection.selected();
        let thread = if position != gtk::INVALID_LIST_POSITION {
            self.model
                .item(position)
                .and_downcast::<glib::BoxedAnyObject>()
                .map(|object| object.borrow::<ThreadSummary>().id)
        } else {
            None
        }?;
        Some((thread, row))
    }

    pub fn set_context(&self, context: ListContext) {
        *self.context.borrow_mut() = context;
    }

    /// Replaces the rows. Only the rows that changed are rebuilt, so the
    /// list keeps its scroll position while mail arrives.
    pub fn set_threads(&self, threads: Vec<ThreadSummary>, open: Option<ThreadId>) {
        self.updating.set(true);
        let old: Vec<ThreadSummary> = (0..self.model.n_items())
            .filter_map(|index| {
                self.model
                    .item(index)
                    .and_downcast::<glib::BoxedAnyObject>()
            })
            .map(|object| object.borrow::<ThreadSummary>().clone())
            .collect();
        let prefix = old.iter().zip(&threads).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(threads[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let removed = old.len() - prefix - suffix;
        let added: Vec<glib::BoxedAnyObject> = threads[prefix..threads.len() - suffix]
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        if removed > 0 || !added.is_empty() {
            self.model.splice(prefix as u32, removed as u32, &added);
        }
        let known: HashSet<ThreadId> = threads.iter().map(|thread| thread.id).collect();
        self.selected.borrow_mut().retain(|id| known.contains(id));
        let position = threads
            .iter()
            .position(|thread| Some(thread.id) == open)
            .filter(|_| !self.selecting.get());
        self.selection
            .set_selected(position.map_or(gtk::INVALID_LIST_POSITION, |index| index as u32));
        self.updating.set(false);
        self.notify_selection();
    }

    pub fn scroll_to_top(&self) {
        self.widget.vadjustment().set_value(0.0);
    }

    pub fn is_empty(&self) -> bool {
        self.model.n_items() == 0
    }

    /// Opens the conversation `delta` rows below (or above) the open one,
    /// as j and k do.
    pub fn open_relative(&self, delta: i32) {
        let count = self.model.n_items();
        if count == 0 || self.selecting.get() {
            return;
        }
        let current = self.selection.selected();
        let next = if current == gtk::INVALID_LIST_POSITION {
            if delta > 0 { 0 } else { count - 1 }
        } else {
            (current as i64 + i64::from(delta)).clamp(0, i64::from(count) - 1) as u32
        };
        if next != current {
            self.selection.set_selected(next);
            if let Some(view) = self.widget.child().and_downcast::<gtk::ListView>() {
                view.scroll_to(next, gtk::ListScrollFlags::FOCUS, None);
            }
        }
    }

    pub fn unselect(&self) {
        self.updating.set(true);
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.updating.set(false);
    }

    pub fn set_selection_mode(&self, on: bool) {
        if self.selecting.get() == on {
            return;
        }
        self.selecting.set(on);
        self.selected.borrow_mut().clear();
        self.anchor.set(None);
        self.unselect();
        self.rebind_all();
        self.refresh_marks();
        self.notify_selection();
    }

    pub fn selection_mode(&self) -> bool {
        self.selecting.get()
    }

    pub fn selected_threads(&self) -> Vec<ThreadId> {
        self.selected.borrow().iter().copied().collect()
    }

    fn toggle_selected(&self, id: ThreadId) {
        {
            let mut selected = self.selected.borrow_mut();
            if !selected.remove(&id) {
                selected.insert(id);
            }
        }
        self.anchor.set(Some(id));
        self.refresh_marks();
        self.notify_selection();
    }

    fn notify_selection(&self) {
        if let Some(callback) = self.on_selection.borrow().as_ref() {
            callback(self.selected.borrow().len());
        }
    }

    fn rebind_all(&self) {
        let items: Vec<glib::Object> = (0..self.model.n_items())
            .filter_map(|index| self.model.item(index))
            .collect();
        let was = self.updating.replace(true);
        self.model.splice(0, items.len() as u32, &items);
        self.updating.set(was);
    }

    fn bind_row(&self, widgets: &RowWidgets, thread: &ThreadSummary) {
        let context = self.context.borrow();
        widgets.thread.set(thread.id);
        let own = |address: &str| address.eq_ignore_ascii_case(&context.own_address);
        let me = gettext("Me");
        let name_of = |name: &str, address: &str, short: bool| -> String {
            if own(address) {
                return me.clone();
            }
            let full = if name.is_empty() { address } else { name };
            if short && !name.is_empty() {
                full.split_whitespace().next().unwrap_or(full).to_owned()
            } else {
                full.to_owned()
            }
        };
        let avatar_sender = thread
            .senders
            .iter()
            .rev()
            .find(|sender| !own(&sender.address))
            .or(thread.senders.last());
        widgets.avatar.set_text(avatar_sender.map(|sender| {
            if sender.name.is_empty() {
                sender.address.as_str()
            } else {
                sender.name.as_str()
            }
        }));

        if context.drafts {
            widgets.sender.set_label(&gettext("Draft"));
            widgets.sender.add_css_class("accent");
        } else {
            widgets.sender.remove_css_class("accent");
            let label = if thread.count > 1 {
                thread
                    .senders
                    .iter()
                    .map(|sender| name_of(&sender.name, &sender.address, true))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                name_of(&thread.latest.from_name, &thread.latest.from_address, false)
            };
            widgets.sender.set_label(&label);
        }
        widgets.time.set_label(&format_time(thread.latest.date));
        let subject = if thread.subject.is_empty() {
            gettext("(No subject)")
        } else {
            thread.subject.clone()
        };
        set_highlighted(&widgets.subject, &subject, &context.highlight);
        match context.folders.get(&thread.id) {
            Some(name) => {
                widgets.folder.set_label(name);
                widgets.folder.set_visible(true);
            }
            None => widgets.folder.set_visible(false),
        }
        // " · 2" after the senders: how many messages the conversation has.
        widgets.count.set_label(&format!(" · {}", thread.count));
        widgets.count.set_visible(thread.count > 1);
        widgets.attachment.set_visible(thread.has_attachment);
        widgets.star.set_visible(thread.flagged);
        widgets.unread_dot.set_visible(thread.unread);
        for label in [&widgets.sender, &widgets.subject] {
            if thread.unread {
                label.add_css_class("heading");
            } else {
                label.remove_css_class("heading");
            }
        }

        let selecting = self.selecting.get();
        widgets.avatar.set_visible(!selecting);
        widgets.check.set_visible(selecting);
        let picked = selecting && self.selected.borrow().contains(&thread.id);
        widgets.check.set_active(picked);
        if picked {
            widgets.root.add_css_class("picked");
        } else {
            widgets.root.remove_css_class("picked");
        }
    }
}

fn build_row(list: &Weak<MessageList>) -> (gtk::Overlay, RowWidgets) {
    let thread = Rc::new(Cell::new(0));
    let avatar = adw::Avatar::new(40, None, true);
    let check = gtk::CheckButton::builder()
        .can_target(false)
        .can_focus(false)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(40)
        .visible(false)
        .build();
    let unread_dot = gtk::Box::builder()
        .css_classes(["unread-dot"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .build();
    unread_dot.update_property(&[gtk::accessible::Property::Label(&gettext("Unread"))]);
    let lead = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    lead.append(&avatar);
    lead.append(&check);
    let avatar_box = gtk::Overlay::builder()
        .child(&lead)
        .valign(gtk::Align::Center)
        .build();
    avatar_box.add_overlay(&unread_dot);

    let label = |classes: &[&str]| {
        gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(
                classes
                    .iter()
                    .copied()
                    .map(String::from)
                    .collect::<Vec<_>>(),
            )
            .build()
    };
    let sender = label(&["sender"]);
    let time = label(&["dim-label", "caption"]);
    time.set_hexpand(true);
    time.set_xalign(1.0);
    time.set_margin_start(6);
    time.set_ellipsize(gtk::pango::EllipsizeMode::None);
    sender.set_hexpand(false);
    let count = label(&["dim-label"]);
    count.set_ellipsize(gtk::pango::EllipsizeMode::None);
    let first_line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    first_line.append(&sender);
    first_line.append(&count);
    first_line.append(&time);
    let subject = label(&["subject"]);
    subject.set_hexpand(true);
    let attachment = gtk::Image::from_icon_name("mail-attachment-symbolic");
    attachment.add_css_class("dim-label");
    attachment.set_tooltip_text(Some(&gettext("Has attachments")));
    let star = gtk::Image::from_icon_name("starred-symbolic");
    star.add_css_class("accent");
    star.set_tooltip_text(Some(&gettext("Starred")));
    let folder = gtk::Label::builder()
        .css_classes(["folder-tag", "caption"])
        .visible(false)
        .build();
    // Every row has exactly two lines; the marks sit right of the subject.
    let second_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    second_line.append(&subject);
    second_line.append(&folder);
    second_line.append(&attachment);
    second_line.append(&star);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&first_line);
    text.append(&second_line);

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .css_classes(["message-row"])
        .build();
    row.append(&avatar_box);
    row.append(&text);

    let overlay = gtk::Overlay::builder()
        .child(&row)
        .css_classes(["message-row-frame"])
        .build();

    // Ctrl click and Shift click pick conversations without opening them.
    let modified = gtk::GestureClick::builder()
        .button(gtk::gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    {
        let list = list.clone();
        let thread = thread.clone();
        modified.connect_pressed(move |gesture, _, _, _| {
            let modifiers = gesture.current_event_state();
            let range = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let toggle = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if !(range || toggle) {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(list) = list.upgrade() {
                list.modified_click(thread.get(), range);
            }
        });
    }
    overlay.add_controller(modified);

    // A double click opens the message in a window of its own; the first
    // click has already opened it in the reading pane.
    let double = gtk::GestureClick::builder()
        .button(gtk::gdk::BUTTON_PRIMARY)
        .build();
    {
        let list = list.clone();
        let thread = thread.clone();
        double.connect_pressed(move |gesture, presses, _, _| {
            let modifiers = gesture.current_event_state();
            if presses != 2
                || modifiers.intersects(
                    gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK,
                )
            {
                return;
            }
            if let Some(list) = list.upgrade() {
                list.open_window(thread.get());
            }
        });
    }
    overlay.add_controller(double);

    // The actions of a row: right click, long press on touch screens.
    let open_menu = {
        let list = list.clone();
        let thread = thread.clone();
        let anchor = overlay.clone();
        move |x: f64, y: f64| {
            if let Some(list) = list.upgrade() {
                list.menu(thread.get(), anchor.upcast_ref(), x, y);
            }
        }
    };
    let right_click = gtk::GestureClick::builder()
        .button(gtk::gdk::BUTTON_SECONDARY)
        .build();
    let menu = open_menu.clone();
    right_click.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        menu(x, y);
    });
    overlay.add_controller(right_click);
    let long_press = gtk::GestureLongPress::builder().touch_only(true).build();
    long_press.connect_pressed(move |gesture, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        open_menu(x, y);
    });
    overlay.add_controller(long_press);

    // Rows can be dragged onto a folder to move them there.
    let drag = gtk::DragSource::builder()
        .actions(gtk::gdk::DragAction::MOVE)
        .build();
    {
        let list = list.clone();
        let thread = thread.clone();
        drag.connect_prepare(move |_, _, _| {
            let list = list.upgrade()?;
            let ids: Vec<String> = list
                .targets(thread.get())
                .iter()
                .map(ThreadId::to_string)
                .collect();
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("{DRAG_PREFIX}{}", ids.join(",")).to_value(),
            ))
        });
    }
    overlay.add_controller(drag);
    (
        overlay.clone(),
        RowWidgets {
            root: overlay,
            thread,
            avatar,
            check,
            unread_dot,
            sender,
            time,
            subject,
            count,
            attachment,
            star,
            folder,
        },
    )
}

/// Shows `text` with the search words marked.
fn set_highlighted(label: &gtk::Label, text: &str, terms: &[String]) {
    let ranges = search::match_ranges(text, terms);
    if ranges.is_empty() {
        label.set_use_markup(false);
        label.set_label(text);
        return;
    }
    let mut markup = String::with_capacity(text.len() + ranges.len() * 40);
    let mut last = 0;
    for (start, end) in ranges {
        markup.push_str(&glib::markup_escape_text(&text[last..start]));
        markup.push_str("<span background=\"#f6d32d\" bgalpha=\"55%\">");
        markup.push_str(&glib::markup_escape_text(&text[start..end]));
        markup.push_str("</span>");
        last = end;
    }
    markup.push_str(&glib::markup_escape_text(&text[last..]));
    label.set_markup(&markup);
}

#[derive(Debug, PartialEq, Eq)]
enum Age {
    Today,
    Yesterday,
    ThisWeek,
    Older,
}

/// How long ago a day was, counted in calendar days.
fn age(days_ago: i64) -> Age {
    match days_ago {
        i64::MIN..=0 => Age::Today,
        1 => Age::Yesterday,
        2..=6 => Age::ThisWeek,
        _ => Age::Older,
    }
}

fn days_between(date: &glib::DateTime, now: &glib::DateTime) -> i64 {
    let day = |d: &glib::DateTime| {
        let (y, m, day) = d.ymd();
        glib::DateTime::from_utc(y, m, day, 12, 0, 0.0)
            .map(|d| d.to_unix() / 86_400)
            .unwrap_or_default()
    };
    day(now) - day(date)
}

/// Like Apple Mail: the time today, "Yesterday", the weekday this week,
/// otherwise a short date.
pub fn format_time(timestamp: i64) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(timestamp) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let format = match age(days_between(&date, &now)) {
        Age::Today => "%R".to_owned(),
        Age::Yesterday => return gettext("Yesterday"),
        Age::ThisWeek => "%A".to_owned(),
        // Translators: short date of an older message, see strftime(3).
        Age::Older => gettext("%-m/%-d/%y"),
    };
    date.format(&format)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// "Today, 13:35", "Yesterday, 08:12" or the full date, for the head of a
/// message.
pub fn format_message_time(timestamp: i64) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(timestamp) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let time = date.format("%R").map(|s| s.to_string()).unwrap_or_default();
    match age(days_between(&date, &now)) {
        // Translators: "Today, 13:35".
        Age::Today => fmt(&gettext("Today, {}"), &[&time]),
        // Translators: "Yesterday, 13:35".
        Age::Yesterday => fmt(&gettext("Yesterday, {}"), &[&time]),
        // Translators: weekday and time of a message from this week.
        Age::ThisWeek => date
            .format(&gettext("%A, %R"))
            .map(|s| s.to_string())
            .unwrap_or_default(),
        // Translators: date and time of an older message, see strftime(3).
        Age::Older => date
            .format(&gettext("%-m/%-d/%y, %R"))
            .map(|s| s.to_string())
            .unwrap_or_default(),
    }
}

/// "11/22/26 at 13:35" in the line that introduces a quoted message.
pub fn format_quote_time(timestamp: i64) -> String {
    glib::DateTime::from_unix_local(timestamp)
        .ok()
        .and_then(|date| {
            // Translators: date in "On {date}, {name} wrote:", see strftime(3).
            date.format(&gettext("%-m/%-d/%y at %R")).ok()
        })
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Full date and time for the reading pane.
pub fn format_long_time(timestamp: i64) -> String {
    glib::DateTime::from_unix_local(timestamp)
        .ok()
        .and_then(|date| {
            // Translators: date and time of a message, see strftime(3).
            date.format(&gettext("%A, %B %-e, %Y, %R")).ok()
        })
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_like_apple_mail() {
        assert_eq!(age(0), Age::Today);
        assert_eq!(age(1), Age::Yesterday);
        assert_eq!(age(6), Age::ThisWeek);
        assert_eq!(age(7), Age::Older);
        let monday = glib::DateTime::from_local(2026, 11, 23, 0, 5, 0.0).unwrap();
        let sunday = glib::DateTime::from_local(2026, 11, 22, 23, 55, 0.0).unwrap();
        assert_eq!(days_between(&sunday, &monday), 1);
        let new_year = glib::DateTime::from_local(2027, 1, 1, 9, 0, 0.0).unwrap();
        let old_year = glib::DateTime::from_local(2026, 12, 31, 18, 0, 0.0).unwrap();
        assert_eq!(days_between(&old_year, &new_year), 1);
    }
}
