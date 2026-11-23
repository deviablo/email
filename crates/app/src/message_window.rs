// SPDX-License-Identifier: GPL-3.0-or-later

//! A single message in a window of its own, opened by double-clicking it in
//! the list. It shows the same card as the reading pane; replies open in
//! the compose window. It belongs to the active account and closes when
//! another account becomes active.

use std::rc::Rc;

use adw::prelude::*;
use e_mail_core::model::{AccountId, MessageId};
use gtk::{gdk, glib};

use crate::reader::Reader;
use crate::window::Window;

pub struct MessageWindow {
    pub window: adw::Window,
    pub reader: Rc<Reader>,
    pub account: AccountId,
    pub message: MessageId,
}

impl MessageWindow {
    pub fn new(host: &Rc<Window>, account: AccountId, message: MessageId) -> Rc<MessageWindow> {
        let reader = Reader::new();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&reader.widget));

        let window = adw::Window::builder()
            .default_width(860)
            .default_height(720)
            .width_request(360)
            .height_request(360)
            .content(&view)
            .build();
        if let Some(app) = host.window.application() {
            window.set_application(Some(&app));
        }

        let this = Rc::new(MessageWindow {
            window,
            reader,
            account,
            message,
        });

        let weak = Rc::downgrade(host);
        let viewer = Rc::downgrade(&this);
        this.reader.connect_action(move |action, id| {
            if let (Some(host), Some(viewer)) = (weak.upgrade(), viewer.upgrade()) {
                host.message_window_action(&viewer, action, id);
            }
        });
        let weak = Rc::downgrade(host);
        let viewer = Rc::downgrade(&this);
        this.reader.connect_need_body(move |id| {
            if let (Some(host), Some(viewer)) = (weak.upgrade(), viewer.upgrade()) {
                host.load_body(&viewer.reader, id);
            }
        });
        let weak = Rc::downgrade(host);
        let viewer = Rc::downgrade(&this);
        this.reader.connect_images(move |id, choice| {
            if let (Some(host), Some(viewer)) = (weak.upgrade(), viewer.upgrade()) {
                host.allow_images(id, choice);
                host.load_body(&viewer.reader, id);
            }
        });
        let weak = Rc::downgrade(host);
        this.reader.connect_attachment(move |id, attachment, open| {
            if let Some(host) = weak.upgrade() {
                host.handle_attachment(id, attachment, open);
            }
        });

        let weak = Rc::downgrade(host);
        this.window.connect_close_request(move |_| {
            if let Some(host) = weak.upgrade() {
                host.message_window_closed(message);
            }
            glib::Propagation::Proceed
        });

        let keys = gtk::EventControllerKey::new();
        let window = this.window.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let ctrl_w = key == gdk::Key::w && modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            if key == gdk::Key::Escape || ctrl_w {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.window.add_controller(keys);
        this
    }

    pub fn present(&self) {
        self.window.present();
    }
}
