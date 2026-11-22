// SPDX-License-Identifier: GPL-3.0-or-later

//! Hostile messages: nothing in them may run, load from the network, leave
//! the attachment folder or slip extra headers into an outgoing message.

use e_mail_core::compose::{self, Draft, Mailbox};
use e_mail_core::html;
use e_mail_core::parse::{self, Body};

fn shown(html_body: &str) -> String {
    let rendered = html::render_html(html_body, &[], false);
    let start = rendered.document.find("<body").unwrap();
    rendered.document[start..].to_lowercase()
}

#[test]
fn nothing_in_a_message_runs() {
    let body = shown(
        r#"<script>alert(1)</script>
<img src="x" onerror="alert(2)">
<a href="javascript:alert(3)">click</a>
<a href="data:text/html,<script>alert(4)</script>">data</a>
<svg><script>alert(5)</script><circle onload="alert(6)"/></svg>
<iframe src="https://evil.example"></iframe>
<object data="https://evil.example/x.swf"></object>
<embed src="https://evil.example/x">
<form action="https://evil.example"><input name="password"></form>
<meta http-equiv="refresh" content="0;url=https://evil.example">
<base href="https://evil.example/">
<body onload="alert(7)">"#,
    );
    for forbidden in [
        "<script",
        "onerror",
        "onload",
        "javascript:",
        "data:text/html",
        "<iframe",
        "<object",
        "<embed",
        "<form",
        "<input",
        "http-equiv",
        "<base",
        "<svg",
    ] {
        assert!(!body.contains(forbidden), "{forbidden} survived: {body}");
    }
}

#[test]
fn nothing_loads_from_the_network_unless_allowed() {
    let rendered = html::render_html(
        r#"<style>@import url(https://evil.example/a.css); body { background: url(https://evil.example/b.png) }</style>
<p style="background-image: url('https://evil.example/c.png')">x</p>
<img src="https://evil.example/pixel.gif"><img src="//evil.example/d.gif">"#,
        &[],
        false,
    );
    assert!(rendered.has_remote_content);
    let document = rendered.document.to_lowercase();
    // The content security policy forbids every network load.
    assert!(document.contains("default-src 'none'"));
    assert!(document.contains("img-src data:;"));
    assert!(!document.contains("src=\"https://evil.example/pixel.gif\""));
    assert!(!document.contains("src=\"//evil.example/d.gif\""));
}

#[test]
fn attachment_names_cannot_escape() {
    let raw = b"From: a@example.org\r\nSubject: x\r\n\
Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
--m\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--m\r\nContent-Type: text/plain; name=\"../../.bashrc\"\r\n\
Content-Disposition: attachment; filename=\"../../.bashrc\"\r\n\r\nrm -rf ~\r\n\
--m\r\nContent-Type: application/x-desktop\r\n\
Content-Disposition: attachment; filename=\"/etc/x.desktop\"\r\n\r\n[Desktop Entry]\r\n\
--m--\r\n";
    let parsed = parse::parse_message(raw);
    let names: Vec<&str> = parsed.attachments.iter().map(|a| a.name.as_str()).collect();
    for name in &names {
        assert!(
            !name.contains('/') && !name.starts_with('.'),
            "unsafe name {name}"
        );
    }
    assert_eq!(names, ["_.._.bashrc", "_etc_x.desktop"]);
}

#[test]
fn broken_and_huge_messages_do_not_crash() {
    let long_subject = format!("Subject: {}\r\n", "A".repeat(200_000));
    let mut raw = long_subject.into_bytes();
    raw.extend_from_slice(b"Content-Type: multipart/mixed; boundary=\"never-closed\"\r\n\r\n");
    raw.extend_from_slice(b"--never-closed\r\nContent-Type: text/html\r\n\r\n<p>half");
    let parsed = parse::parse_message(&raw);
    assert!(parsed.header.subject.len() >= 200_000);
    let _ = html::render_body(&parsed.body, &parsed.inline_images, false);
    let garbage: Vec<u8> = (0..5000u32).map(|i| (i * 7919 % 251) as u8).collect();
    let parsed = parse::parse_message(&garbage);
    assert!(matches!(
        parsed.body,
        Body::Text(_) | Body::Html(_) | Body::Empty
    ));
    let _ = parse::parse_header(&garbage);
}

#[test]
fn no_header_injection_when_sending() {
    // Addresses with line breaks are refused before anything is built.
    assert!(compose::parse_addresses("victim@example.org\r\nBcc: all@example.org").is_err());
    assert!(compose::parse_mailbox("a@example.org\nX: y").is_none());

    let draft = Draft {
        message_id: "id@example.org".into(),
        to: vec![Mailbox::new(
            "Eve\r\nBcc: all@example.org",
            "eve@example.org",
        )],
        subject: "Hello\r\nBcc: everyone@example.org\r\nX-Injected: yes".into(),
        html: "<p>x</p>".into(),
        ..Draft::default()
    };
    let raw = compose::build_message(&Mailbox::new("Max", "max@example.org"), &draft, 0);
    let text = String::from_utf8_lossy(&raw);
    let header_end = text.find("\r\n\r\n").unwrap();
    let header = &text[..header_end];
    for line in header.lines() {
        assert!(!line.starts_with("Bcc:"), "injected Bcc: {header}");
        assert!(
            !line.starts_with("X-Injected:"),
            "injected header: {header}"
        );
    }
}
