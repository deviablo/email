// SPDX-License-Identifier: GPL-3.0-or-later

//! The text editor shared by new mail, replies and signatures: WebKit in
//! editing mode with a small formatting bar (bold, italic, link, list,
//! picture). Pictures live in the text as `data:` URIs; when the message
//! is built they become embedded parts.
//!
//! Like the reading pane, scripts in the content never run. Only our own
//! script in a separate world reports changes and turns pasted pictures
//! into `data:` URIs, and the page cannot load anything from the network.

use std::cell::RefCell;
use std::future::Future;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use e_mail_core::compose;
use gettextrs::gettext;
use gtk::{gio, glib};
use webkit::prelude::*;

use crate::reader;

const WORLD: &str = "e-mail-editor";
const SCRIPT: &str = "(() => {
  const post = () => window.webkit.messageHandlers.changed.postMessage(0);
  document.addEventListener('input', post);
  document.addEventListener('paste', (event) => {
    const items = event.clipboardData ? Array.from(event.clipboardData.items) : [];
    const image = items.find(item => item.kind === 'file' && item.type.startsWith('image/'));
    if (!image) return;
    event.preventDefault();
    const reader = new FileReader();
    reader.onload = () => { document.execCommand('insertImage', false, reader.result); post(); };
    reader.readAsDataURL(image.getAsFile());
  });
  document.addEventListener('drop', (event) => {
    const files = event.dataTransfer ? Array.from(event.dataTransfer.files) : [];
    if (files.length === 0) return;
    event.preventDefault();
  });
  // Pictures get a frame and a handle when clicked; dragging the handle
  // changes the width and the height follows, so nothing is distorted.
  // Both live outside <body>, so they never end up in the message.
  let picture = null;
  const make = (css) => {
    const element = document.createElement('div');
    element.setAttribute('contenteditable', 'false');
    element.style.cssText = 'position:absolute;display:none;box-sizing:border-box;z-index:10;' + css;
    document.documentElement.appendChild(element);
    return element;
  };
  const frame = make('border:2px solid #3584e4;pointer-events:none;');
  const handle = make('width:14px;height:14px;border-radius:4px;background:#3584e4;' +
    'border:2px solid #fff;cursor:nwse-resize;');
  const hide = () => { picture = null; frame.style.display = handle.style.display = 'none'; };
  const place = () => {
    if (!picture || !document.body.contains(picture)) { hide(); return; }
    const r = picture.getBoundingClientRect();
    frame.style.display = handle.style.display = 'block';
    frame.style.left = (r.left + scrollX) + 'px';
    frame.style.top = (r.top + scrollY) + 'px';
    frame.style.width = r.width + 'px';
    frame.style.height = r.height + 'px';
    handle.style.left = (r.right + scrollX - 9) + 'px';
    handle.style.top = (r.bottom + scrollY - 9) + 'px';
  };
  document.addEventListener('click', (event) => {
    if (event.target instanceof HTMLImageElement && document.body.contains(event.target)) {
      picture = event.target;
      place();
    } else if (event.target !== handle) {
      hide();
    }
  });
  handle.addEventListener('mousedown', (event) => {
    if (!picture) return;
    event.preventDefault();
    const start = event.clientX;
    const width = picture.getBoundingClientRect().width;
    const widest = Math.max(40, document.body.clientWidth - 28);
    const move = (e) => {
      const w = Math.max(24, Math.min(widest, width + e.clientX - start));
      picture.removeAttribute('width');
      picture.removeAttribute('height');
      picture.style.width = Math.round(w) + 'px';
      picture.style.height = 'auto';
      place();
    };
    const up = () => {
      document.removeEventListener('mousemove', move);
      document.removeEventListener('mouseup', up);
      post();
    };
    document.addEventListener('mousemove', move);
    document.addEventListener('mouseup', up);
  });
  document.addEventListener('input', place);
  document.addEventListener('keydown', (event) => { if (event.key === 'Escape') hide(); });
  window.addEventListener('resize', place);
})();";

type Callback = Box<dyn Fn()>;

pub struct Editor {
    pub web: webkit::WebView,
    /// The formatting bar, to be placed below the text.
    pub toolbar: gtk::Box,
    /// Room at the end of the formatting bar for the owner's buttons.
    pub bar: gtk::Box,
    on_changed: RefCell<Option<Callback>>,
    weak: RefCell<Weak<Editor>>,
}

impl Editor {
    pub fn new(html: &str) -> Rc<Editor> {
        let manager = webkit::UserContentManager::new();
        manager.register_script_message_handler("changed", Some(WORLD));
        manager.add_script(&webkit::UserScript::for_world(
            SCRIPT,
            webkit::UserContentInjectedFrames::TopFrame,
            webkit::UserScriptInjectionTime::End,
            WORLD,
            &[],
            &[],
        ));
        let web = webkit::WebView::builder()
            .user_content_manager(&manager)
            .network_session(&reader::network_session())
            .editable(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        reader::configure(&web);
        web.update_property(&[gtk::accessible::Property::Label(&gettext("Message"))]);

        let bar = gtk::Box::builder()
            .spacing(2)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .margin_bottom(4)
            .hexpand(true)
            .build();
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["editor-bar"])
            .build();
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        toolbar.append(&bar);

        let this = Rc::new(Editor {
            web,
            toolbar,
            bar,
            on_changed: RefCell::default(),
            weak: RefCell::default(),
        });
        *this.weak.borrow_mut() = Rc::downgrade(&this);

        let weak = Rc::downgrade(&this);
        manager.connect_script_message_received(Some("changed"), move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.changed();
            }
        });
        this.add_format_buttons();
        this.set_html(html);

        let weak = Rc::downgrade(&this);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply_colors();
            }
        });
        this
    }

    fn add_format_buttons(&self) {
        let icon_button = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .build()
        };
        let bold = icon_button("format-text-bold-symbolic", &gettext("Bold (Ctrl+B)"));
        let italic = icon_button("format-text-italic-symbolic", &gettext("Italic (Ctrl+I)"));
        let link = icon_button(
            "io.github.digistalter.EMail-link-symbolic",
            &gettext("Insert Link"),
        );
        let list = icon_button("view-list-bullet-symbolic", &gettext("Bulleted List"));
        let image = icon_button(
            "image-x-generic-symbolic",
            &gettext("Insert Picture into the Text"),
        );
        for (button, command) in [
            (&bold, "Bold"),
            (&italic, "Italic"),
            (&list, "InsertUnorderedList"),
        ] {
            let weak = self.weak.borrow().clone();
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.web.grab_focus();
                    this.web.execute_editing_command(command);
                    this.changed();
                }
            });
        }
        let weak = self.weak.borrow().clone();
        link.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.ask_for_link();
            }
        });
        let weak = self.weak.borrow().clone();
        image.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_picture();
            }
        });
        let separator = gtk::Separator::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(4)
            .margin_end(4)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        for widget in [
            bold.upcast_ref::<gtk::Widget>(),
            italic.upcast_ref(),
            link.upcast_ref(),
            list.upcast_ref(),
            separator.upcast_ref(),
            image.upcast_ref(),
        ] {
            self.bar.append(widget);
        }
    }

    pub fn connect_changed(&self, callback: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(callback));
    }

    fn changed(&self) {
        if let Some(callback) = self.on_changed.borrow().as_ref() {
            callback();
        }
    }

    /// Replaces the whole text.
    pub fn set_html(&self, html: &str) {
        self.web.load_html(&document(html, &self.colors()), None);
    }

    /// The current text as HTML for the inside of `<body>`.
    pub fn html(&self) -> impl Future<Output = String> + 'static {
        // Asked right away, so the answer comes even if the editor leaves
        // the screen in the meantime.
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.web.evaluate_javascript(
            "document.body.innerHTML",
            Some(WORLD),
            None,
            gio::Cancellable::NONE,
            move |result| {
                let html = match result {
                    Ok(value) => value.to_str().to_string(),
                    Err(error) => {
                        tracing::error!("could not read the text of the editor: {error}");
                        String::new()
                    }
                };
                let _ = sender.send(html);
            },
        );
        async move { receiver.await.unwrap_or_default() }
    }

    /// Puts the cursor at the start of the text and focuses it.
    pub fn focus_start(&self) {
        let web = self.web.clone();
        let focus = move |web: &webkit::WebView| {
            web.grab_focus();
            web.evaluate_javascript(
                "const r = document.createRange(); r.setStart(document.body, 0); \
                 r.collapse(true); const s = getSelection(); s.removeAllRanges(); \
                 s.addRange(r); document.body.focus();",
                Some(WORLD),
                None,
                gio::Cancellable::NONE,
                |_| {},
            );
        };
        if web.is_loading() {
            let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::default();
            let slot = handler.clone();
            let id = web.connect_load_changed(move |web, event| {
                if event == webkit::LoadEvent::Finished {
                    focus(web);
                    if let Some(id) = slot.take() {
                        web.disconnect(id);
                    }
                }
            });
            *handler.borrow_mut() = Some(id);
        } else {
            focus(&web);
        }
    }

    /// The text colours of libadwaita. The widget's own colour is not
    /// known yet while the editor is being built.
    fn colors(&self) -> Colors {
        let dark = adw::StyleManager::default().is_dark();
        Colors {
            text: if dark { "#ffffff" } else { "rgba(0,0,6,0.8)" }.to_owned(),
            link: if dark { "#78aeed" } else { "#1c71d8" }.to_owned(),
        }
    }

    fn apply_colors(&self) {
        let colors = self.colors();
        self.web.evaluate_javascript(
            &format!(
                "document.documentElement.style.setProperty('--text', '{}'); \
                 document.documentElement.style.setProperty('--link', '{}');",
                colors.text, colors.link
            ),
            Some(WORLD),
            None,
            gio::Cancellable::NONE,
            |_| {},
        );
    }

    fn ask_for_link(&self) {
        let entry = gtk::Entry::builder()
            .placeholder_text("https://")
            .activates_default(true)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Insert Link"))
            .body(gettext("Enter the web address the link leads to."))
            .extra_child(&entry)
            .default_response("insert")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("insert", &gettext("Insert")),
        ]);
        dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
        let weak = self.weak.borrow().clone();
        let field = entry.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if response != "insert" {
                return;
            }
            let Some(url) = link_target(&field.text()) else {
                return;
            };
            this.web.grab_focus();
            let script = format!(
                "(() => {{ const url = {url}; const s = getSelection(); \
                 if (s.isCollapsed) {{ const a = document.createElement('a'); a.href = url; \
                 a.textContent = url; s.getRangeAt(0).insertNode(a); s.collapse(a, 1); }} \
                 else {{ document.execCommand('createLink', false, url); }} }})();",
                url = js_string(&url)
            );
            let weak = this.weak.borrow().clone();
            this.web.evaluate_javascript(
                &script,
                Some(WORLD),
                None,
                gio::Cancellable::NONE,
                move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.changed();
                    }
                },
            );
        });
        dialog.present(Some(&self.web));
        entry.grab_focus();
    }

    fn choose_picture(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&gettext("Pictures")));
        filter.add_mime_type("image/png");
        filter.add_mime_type("image/jpeg");
        filter.add_mime_type("image/gif");
        filter.add_mime_type("image/webp");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Insert Picture"))
            .filters(&filters)
            .modal(true)
            .build();
        let window = self.web.root().and_downcast::<gtk::Window>();
        let weak = self.weak.borrow().clone();
        dialog.open(window.as_ref(), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            glib::spawn_future_local(async move {
                let Ok((data, _)) = file.load_contents_future().await else {
                    return;
                };
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let name = file
                    .basename()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let uri = compose::data_uri(compose::mime_type_for(&name), &data);
                this.web.grab_focus();
                this.web
                    .execute_editing_command_with_argument("InsertImage", &uri);
                this.changed();
            });
        });
    }
}

struct Colors {
    text: String,
    link: String,
}

fn document(html: &str, colors: &Colors) -> String {
    let font = gtk::Settings::default()
        .and_then(|settings| settings.gtk_font_name())
        .map(|name| gtk::pango::FontDescription::from_string(&name))
        .map(|font| {
            let size = f64::from(font.size()) / f64::from(gtk::pango::SCALE);
            let family = font.family().map(|f| f.to_string()).unwrap_or_default();
            (family, if size > 0.0 { size } else { 11.0 })
        })
        .unwrap_or_else(|| (String::new(), 11.0));
    let family = font.0.replace(['"', '\\', '<', '>', ';', '}'], "");
    format!(
        "<!DOCTYPE html><html style=\"--text:{text};--link:{link}\"><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; \
style-src 'unsafe-inline'\">\
<style>html{{height:100%;color-scheme:light dark}}\
body{{box-sizing:border-box;min-height:100%;margin:0;padding:12px 14px;\
font-family:\"{family}\",sans-serif;font-size:{size}pt;line-height:1.5;\
overflow-wrap:anywhere;color:var(--text);caret-color:var(--text);outline:none}}\
a{{color:var(--link)}}img{{max-width:100%;height:auto}}\
ul,ol{{margin:4px 0;padding-left:22px}}\
blockquote{{margin:4px 0 4px 2px;padding-left:12px;border-left:3px solid rgba(127,127,127,.45)}}\
.signature{{opacity:.8}}</style></head><body>{html}</body></html>",
        text = colors.text,
        link = colors.link,
        size = font.1,
    )
}

/// A web or mail address from what the user typed, with "https://" added
/// when it was left out.
pub fn link_target(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.contains(char::is_whitespace) {
        return None;
    }
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
    {
        Some(input.to_owned())
    } else if compose::is_valid_address(input) {
        Some(format!("mailto:{input}"))
    } else {
        Some(format!("https://{input}"))
    }
}

/// A JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_get_a_scheme() {
        assert_eq!(
            link_target(" example.org/x ").as_deref(),
            Some("https://example.org/x")
        );
        assert_eq!(link_target("http://a.b").as_deref(), Some("http://a.b"));
        assert_eq!(
            link_target("lena@example.org").as_deref(),
            Some("mailto:lena@example.org")
        );
        assert_eq!(link_target("two words"), None);
        assert_eq!(link_target(""), None);
    }

    #[test]
    fn strings_are_escaped_for_scripts() {
        assert_eq!(
            js_string("a\"b\\c</script>\n"),
            "\"a\\\"b\\\\c\\u003c/script>\\n\""
        );
    }
}
