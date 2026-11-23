// SPDX-License-Identifier: GPL-3.0-or-later

//! Lays children out side by side at their natural width and wraps to a
//! new line at the edge, like words in a paragraph. `GtkFlowBox` lines its
//! children up in columns, which left wide gaps between attachments.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

const SPACING: i32 = 8;
const LINE_SPACING: i32 = 8;

glib::wrapper! {
    pub struct WrapLayout(ObjectSubclass<imp::WrapLayout>)
        @extends gtk::LayoutManager;
}

impl WrapLayout {
    pub fn new() -> WrapLayout {
        glib::Object::new()
    }
}

fn children(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut list = Vec::new();
    let mut child = widget.first_child();
    while let Some(current) = child {
        child = current.next_sibling();
        if current.should_layout() {
            list.push(current);
        }
    }
    list
}

/// Where each child goes for the given width: (child, x, y, width, height).
fn place(widget: &gtk::Widget, width: i32) -> Vec<(gtk::Widget, i32, i32, i32, i32)> {
    let mut placed = Vec::new();
    let (mut x, mut y, mut line_height) = (0, 0, 0);
    for child in children(widget) {
        let (minimum, natural, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        let w = natural.min(width).max(minimum);
        let (_, h, _, _) = child.measure(gtk::Orientation::Vertical, w);
        if x > 0 && x + w > width {
            x = 0;
            y += line_height + LINE_SPACING;
            line_height = 0;
        }
        placed.push((child, x, y, w, h));
        x += w + SPACING;
        line_height = line_height.max(h);
    }
    placed
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct WrapLayout;

    #[glib::object_subclass]
    impl ObjectSubclass for WrapLayout {
        const NAME: &'static str = "EMailWrapLayout";
        type Type = super::WrapLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for WrapLayout {}

    impl LayoutManagerImpl for WrapLayout {
        fn request_mode(&self, _widget: &gtk::Widget) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(
            &self,
            widget: &gtk::Widget,
            orientation: gtk::Orientation,
            for_size: i32,
        ) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let mut minimum = 0;
                let mut natural = 0;
                for (index, child) in children(widget).iter().enumerate() {
                    let (min, nat, _, _) = child.measure(orientation, -1);
                    minimum = minimum.max(min);
                    natural += nat + if index > 0 { SPACING } else { 0 };
                }
                (minimum, natural, -1, -1)
            } else {
                let width = if for_size < 0 { i32::MAX } else { for_size };
                let height = place(widget, width)
                    .iter()
                    .map(|(_, _, y, _, h)| y + h)
                    .max()
                    .unwrap_or(0);
                (height, height, -1, -1)
            }
        }

        fn allocate(&self, widget: &gtk::Widget, width: i32, _height: i32, _baseline: i32) {
            for (child, x, y, w, h) in place(widget, width) {
                child.size_allocate(&gtk::Allocation::new(x, y, w, h), -1);
            }
        }
    }
}
