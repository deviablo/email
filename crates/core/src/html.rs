// SPDX-License-Identifier: GPL-3.0-or-later

//! Turns message bodies into self-contained HTML documents that are safe to
//! show in the reading pane. Scripts and event handlers are removed, and
//! nothing is loaded from the network unless the user allows it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;

use crate::parse::{Body, InlineImage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub document: String,
    /// True if the message wanted to load something from the internet.
    pub has_remote_content: bool,
}

pub fn render_body(body: &Body, inline_images: &[InlineImage], allow_remote: bool) -> Rendered {
    match body {
        Body::Html(html) => render_html(html, inline_images, allow_remote),
        Body::Text(text) => Rendered {
            document: render_text(text),
            has_remote_content: false,
        },
        Body::Empty => Rendered {
            document: render_text(""),
            has_remote_content: false,
        },
    }
}

pub fn render_html(html: &str, inline_images: &[InlineImage], allow_remote: bool) -> Rendered {
    let (clean, has_remote_content) = clean_html(html, inline_images, allow_remote);
    let image_sources = if allow_remote {
        "data: http: https:"
    } else {
        "data:"
    };
    let document = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
img-src {image_sources}; style-src 'unsafe-inline'; font-src data:\">\
<style>body{{margin:0;padding:16px;overflow-wrap:anywhere;font-family:sans-serif}}\
img{{max-width:100%;height:auto}}</style></head><body class=\"mail-html\">{clean}</body></html>"
    );
    Rendered {
        document,
        has_remote_content,
    }
}

/// Sanitised HTML of a message for quoting it in a reply or forward.
/// Embedded pictures become `data:` URIs; remote pictures are kept, the
/// editor does not load them.
pub fn clean_fragment(html: &str, inline_images: &[InlineImage]) -> String {
    clean_html(html, inline_images, true).0
}

/// Sanitised HTML for the inside of `<body>`, and whether the message
/// wanted to load something from the internet.
fn clean_html(html: &str, inline_images: &[InlineImage], allow_remote: bool) -> (String, bool) {
    let images: HashMap<String, String> = inline_images
        .iter()
        .map(|image| {
            let data = base64::engine::general_purpose::STANDARD.encode(&image.data);
            (
                image.content_id.clone(),
                format!("data:{};base64,{data}", image.mime_type),
            )
        })
        .collect();
    let has_remote = Arc::new(AtomicBool::new(false));
    let flag = has_remote.clone();

    let clean = ammonia::Builder::default()
        .add_tags(["style", "font", "center"])
        .rm_clean_content_tags(["style"])
        // The document title often repeats the subject; without its tag
        // the text would show up at the top of the message.
        .add_clean_content_tags(["title"])
        .add_generic_attributes([
            "style",
            "align",
            "valign",
            "bgcolor",
            "width",
            "height",
            "border",
            "cellpadding",
            "cellspacing",
            "color",
            "face",
            "size",
            "dir",
        ])
        .add_url_schemes(["cid", "data"])
        .attribute_filter(move |element, attribute, value| {
            if attribute == "src" {
                if let Some(id) = value.strip_prefix("cid:") {
                    return images.get(id).map(|uri| Cow::Owned(uri.clone()));
                }
                if is_remote(value) {
                    flag.store(true, Ordering::Relaxed);
                    if !allow_remote {
                        return None;
                    }
                }
                if element != "img" && value.starts_with("data:") {
                    return None;
                }
            }
            // Links to inline documents could only be a trick.
            if attribute == "href" && value.trim_start().to_ascii_lowercase().starts_with("data:") {
                return None;
            }
            if attribute == "style" && mentions_remote_url(value) {
                flag.store(true, Ordering::Relaxed);
            }
            Some(Cow::Borrowed(value))
        })
        .clean(html)
        .to_string();

    if clean.contains("<style") && mentions_remote_url(&clean) {
        has_remote.store(true, Ordering::Relaxed);
    }
    (clean, has_remote.load(Ordering::Relaxed))
}

/// One message of a conversation for printing.
pub struct PrintMessage<'a> {
    pub subject: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub date: &'a str,
    pub body: &'a Body,
    pub inline_images: &'a [InlineImage],
}

/// A single document with all messages of a conversation, each below its
/// sender, recipients and date. Nothing is loaded from the internet.
pub fn print_document(messages: &[PrintMessage<'_>], labels: [&str; 2]) -> String {
    let mut out = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; \
style-src 'unsafe-inline'; font-src data:\">\
<style>body{margin:0;font-family:sans-serif;overflow-wrap:anywhere}\
img{max-width:100%;height:auto}.message{margin-bottom:24px;page-break-inside:auto}\
.head{border-bottom:1px solid #999;padding-bottom:6px;margin-bottom:10px}\
.head p{margin:2px 0}pre{white-space:pre-wrap;font-family:inherit;margin:0}</style>\
</head><body>",
    );
    for (index, message) in messages.iter().enumerate() {
        if index == 0 {
            out.push_str(&format!("<h1>{}</h1>", escape(message.subject)));
        }
        out.push_str(&format!(
            "<div class=\"message\"><div class=\"head\"><p><b>{}</b></p><p>{} {}</p><p>{}</p></div>",
            escape(message.from),
            escape(labels[0]),
            escape(message.to),
            escape(message.date),
        ));
        match message.body {
            Body::Html(html) => out.push_str(&clean_html(html, message.inline_images, false).0),
            Body::Text(text) => out.push_str(&format!("<pre>{}</pre>", escape(text))),
            Body::Empty => out.push_str(&format!("<p><i>{}</i></p>", escape(labels[1]))),
        }
        out.push_str("</div>");
    }
    out.push_str("</body></html>");
    out
}

pub fn render_text(text: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\
<style>:root{{color-scheme:light dark}}body{{margin:0;padding:16px;font-family:sans-serif}}\
pre{{white-space:pre-wrap;overflow-wrap:anywhere;font-family:inherit;margin:0}}</style>\
</head><body class=\"mail-text\"><pre>{}</pre></body></html>",
        escape(text)
    )
}

/// The address a link really leads to, as shown while the pointer is on
/// it. Nothing is hidden: user names or passwords in the address and
/// internationalised (punycode) host names stay visible and are pointed out.
pub fn link_target_text(uri: &str) -> String {
    let uri = uri.trim();
    let Some((_, rest)) = uri.split_once("://") else {
        return uri.to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (userinfo, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    let mut notes = Vec::new();
    if userinfo.is_some() {
        notes.push(format!("⚠ {host}"));
    }
    if host
        .split('.')
        .any(|label| label.to_ascii_lowercase().starts_with("xn--"))
    {
        notes.push("⚠ xn--".to_owned());
    }
    if notes.is_empty() {
        uri.to_owned()
    } else {
        format!("{uri}  ({})", notes.join(", "))
    }
}

fn is_remote(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("//")
}

fn mentions_remote_url(css: &str) -> bool {
    let lower = css.to_ascii_lowercase();
    lower.match_indices("url(").any(|(index, _)| {
        let rest = lower[index + 4..].trim_start_matches([' ', '\'', '"']);
        is_remote(rest)
    }) || lower.contains("@import")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(document: &str) -> &str {
        let open = document.find("<body").unwrap();
        let start = open + document[open..].find('>').unwrap() + 1;
        let end = document.rfind("</body>").unwrap();
        &document[start..end]
    }

    #[test]
    fn removes_scripts_and_handlers() {
        let rendered = render_html(
            "<p onclick=\"steal()\">Hi</p><script>alert(1)</script><a href=\"javascript:x()\">x</a>",
            &[],
            false,
        );
        let body = body_of(&rendered.document);
        assert!(!body.contains("script"));
        assert!(!body.contains("onclick"));
        assert!(!body.contains("javascript:"));
        assert!(body.contains("<p>Hi</p>"));
    }

    #[test]
    fn drops_the_document_title() {
        let rendered = render_html(
            "<html><head><title>Repeated subject</title></head><body><p>Hello</p></body></html>",
            &[],
            false,
        );
        let body = body_of(&rendered.document);
        assert!(!body.contains("Repeated subject"));
        assert!(body.contains("<p>Hello</p>"));
    }

    #[test]
    fn blocks_remote_images_by_default() {
        let rendered = render_html(
            "<img src=\"https://tracker.example/pixel.gif\" alt=\"\">",
            &[],
            false,
        );
        assert!(rendered.has_remote_content);
        assert!(!rendered.document.contains("tracker.example"));
        assert!(rendered.document.contains("img-src data:;"));
    }

    #[test]
    fn shows_remote_images_when_allowed() {
        let rendered = render_html("<img src=\"https://example.org/a.png\">", &[], true);
        assert!(rendered.has_remote_content);
        assert!(rendered.document.contains("https://example.org/a.png"));
        assert!(rendered.document.contains("img-src data: http: https:"));
    }

    #[test]
    fn notices_remote_css() {
        let rendered = render_html(
            "<div style=\"background: url('https://example.org/bg.png')\">x</div>",
            &[],
            false,
        );
        assert!(rendered.has_remote_content);
        let rendered = render_html(
            "<style>body{background:url(http://example.org/x)}</style><p>x</p>",
            &[],
            false,
        );
        assert!(rendered.has_remote_content);
        assert!(rendered.document.contains("<style>body{background"));
    }

    #[test]
    fn local_mail_has_no_remote_content() {
        let rendered = render_html("<p style=\"color:red\">Hello</p>", &[], false);
        assert!(!rendered.has_remote_content);
        assert!(rendered.document.contains("style=\"color:red\""));
    }

    #[test]
    fn inlines_cid_images() {
        let image = InlineImage {
            content_id: "logo@x".into(),
            mime_type: "image/png".into(),
            data: b"hello".to_vec(),
        };
        let rendered = render_html("<img src=\"cid:logo@x\">", &[image], false);
        assert!(
            rendered
                .document
                .contains("src=\"data:image/png;base64,aGVsbG8=\"")
        );
        assert!(!rendered.has_remote_content);
    }

    #[test]
    fn unknown_cid_is_dropped() {
        let rendered = render_html("<img src=\"cid:missing\">", &[], false);
        assert!(!rendered.document.contains("cid:"));
    }

    #[test]
    fn print_document_holds_every_message_without_remote_content() {
        let first = Body::Html(
            "<p>Hi</p><img src=\"https://tracker.example/x.gif\"><script>x()</script>".into(),
        );
        let second = Body::Text("Fine & you?".into());
        let document = print_document(
            &[
                PrintMessage {
                    subject: "Plan",
                    from: "Anna <anna@example.org>",
                    to: "me",
                    date: "Monday",
                    body: &first,
                    inline_images: &[],
                },
                PrintMessage {
                    subject: "Re: Plan",
                    from: "Me",
                    to: "Anna",
                    date: "Tuesday",
                    body: &second,
                    inline_images: &[],
                },
            ],
            ["to", "(empty)"],
        );
        assert!(document.contains("<h1>Plan</h1>"));
        assert!(document.contains("Anna &lt;anna@example.org&gt;"));
        assert!(document.contains("<p>Hi</p>"));
        assert!(document.contains("Fine &amp; you?"));
        assert!(!document.contains("tracker.example"));
        assert!(!document.contains("script"));
    }

    #[test]
    fn text_is_escaped() {
        let document = render_text("a <b> & \"c\"");
        assert!(document.contains("<pre>a &lt;b&gt; &amp; &quot;c&quot;</pre>"));
    }

    #[test]
    fn link_targets_show_tricks() {
        assert_eq!(
            link_target_text("https://bank.example/login"),
            "https://bank.example/login"
        );
        assert_eq!(
            link_target_text("https://bank.example@evil.example/"),
            "https://bank.example@evil.example/  (⚠ evil.example)"
        );
        assert!(link_target_text("https://xn--bnk-sna.example/").contains("⚠ xn--"));
        assert_eq!(link_target_text("mailto:a@b.example"), "mailto:a@b.example");
    }
}
