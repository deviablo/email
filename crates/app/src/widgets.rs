// SPDX-License-Identifier: GPL-3.0-or-later

//! Small building blocks shared by the setup, the settings and the window.

use adw::prelude::*;
use e_mail_core::sync::AccountStatus;
use gettextrs::gettext;

/// Fills the `{}` placeholders of a translated text in order. Translators
/// keep the placeholders but may move them.
pub fn fmt(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut args = args.iter();
    let mut rest = template;
    while let Some(index) = rest.find("{}") {
        out.push_str(&rest[..index]);
        out.push_str(args.next().copied().unwrap_or_default());
        rest = &rest[index + 2..];
    }
    out.push_str(rest);
    out
}

pub fn icon_label_button(icon: &str, label: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder()
        .icon_name(icon)
        .label(label)
        .use_underline(true)
        .build();
    gtk::Button::builder().child(&content).build()
}

pub fn boxed_list(rows: &[&gtk::Widget]) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for row in rows {
        list.append(*row);
    }
    list
}

/// A boxed row with a letter avatar, for providers and accounts.
pub fn card(
    avatar_text: &str,
    title: &str,
    subtitle: &str,
    icon: Option<&str>,
    icon_class: Option<&str>,
) -> gtk::ListBox {
    let row = adw::ActionRow::builder()
        .title(glib_escape(title))
        .subtitle(glib_escape(subtitle))
        .activatable(false)
        .build();
    row.add_prefix(&adw::Avatar::new(40, Some(avatar_text), true));
    if let Some(icon) = icon {
        let image = gtk::Image::from_icon_name(icon);
        if let Some(class) = icon_class {
            image.add_css_class(class);
        }
        row.add_suffix(&image);
    }
    boxed_list(&[row.upcast_ref()])
}

pub fn card_row(card: &gtk::ListBox) -> adw::ActionRow {
    card.first_child()
        .and_downcast()
        .expect("a card holds one row")
}

fn glib_escape(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

/// A dimmed line of text with an icon in front.
pub fn note(icon: &str, text: &str) -> gtk::Box {
    let note = gtk::Box::builder()
        .spacing(8)
        .css_classes(["dim-label", "setup-note"])
        .build();
    note.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .valign(gtk::Align::Start)
            .build(),
    );
    note.append(
        &gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["caption"])
            .build(),
    );
    note
}

/// A large button with an icon, a title and an explanation.
pub fn tile(icon: &str, title: &str, text: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(24)
            .halign(gtk::Align::Start)
            .build(),
    );
    content.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build(),
    );
    content.append(
        &gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    gtk::Button::builder()
        .child(&content)
        .css_classes(["card", "setup-tile"])
        .build()
}

/// An entry with a help text below it.
pub fn field(row: &adw::EntryRow, help: &str) -> gtk::Box {
    let field = gtk::Box::new(gtk::Orientation::Vertical, 6);
    field.append(&boxed_list(&[row.upcast_ref()]));
    field.append(
        &gtk::Label::builder()
            .label(help)
            .xalign(0.0)
            .wrap(true)
            .margin_start(12)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    field
}

pub fn status_text(status: Option<&AccountStatus>) -> String {
    match status {
        None | Some(AccountStatus::Connecting) => gettext("Connecting…"),
        Some(AccountStatus::Online) => gettext("Connected"),
        Some(AccountStatus::Offline(_)) => gettext("Offline"),
        Some(AccountStatus::LoginFailed(_)) => gettext("Sign-in failed"),
    }
}

fn status_class(status: Option<&AccountStatus>) -> &'static str {
    match status {
        None | Some(AccountStatus::Connecting) => "busy",
        Some(AccountStatus::Online) => "ok",
        Some(AccountStatus::Offline(_) | AccountStatus::LoginFailed(_)) => "error",
    }
}

/// The small coloured dot that shows the sync state of an account.
pub fn status_dot(status: Option<&AccountStatus>) -> gtk::Box {
    let dot = gtk::Box::builder()
        .css_classes(["status-dot", status_class(status)])
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .build();
    dot.set_tooltip_text(Some(&status_text(status)));
    // Only a problem is worth a mark; all is well needs none.
    dot.set_visible(status_class(status) == "error");
    dot
}

/// Avatar with the account's initials and its sync state as a dot.
pub fn account_avatar(label: &str, size: i32, status: Option<&AccountStatus>) -> gtk::Overlay {
    let overlay = gtk::Overlay::builder()
        .child(&adw::Avatar::new(size, Some(label), true))
        .valign(gtk::Align::Center)
        .build();
    overlay.add_overlay(&status_dot(status));
    overlay
}

#[cfg(test)]
mod tests {
    use super::fmt;

    #[test]
    fn fills_placeholders() {
        assert_eq!(fmt("Allow access at {}", &["GMX"]), "Allow access at GMX");
        assert_eq!(fmt("{} von {}", &["1", "4"]), "1 von 4");
        assert_eq!(fmt("No placeholder", &["x"]), "No placeholder");
        assert_eq!(fmt("{} and {}", &["a"]), "a and ");
    }
}
