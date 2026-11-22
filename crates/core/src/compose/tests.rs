// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use mail_parser::{MessageParser, MimeHeaders, PartType};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake picture";

fn me() -> Mailbox {
    Mailbox::new("Max Berger", "max@example.org")
}

fn texts() -> QuoteTexts {
    QuoteTexts {
        wrote: "On {date}, {name} wrote:".into(),
        forwarded: "Forwarded message".into(),
        from: "From".into(),
        date: "Date".into(),
        subject: "Subject".into(),
        to: "To".into(),
        format_date: Box::new(|date| format!("day {date}")),
    }
}

fn draft() -> Draft {
    Draft {
        message_id: "abc@example.org".into(),
        to: vec![Mailbox::new("Lena Vogt", "lena@example.org")],
        cc: vec![Mailbox::new("", "tom@example.org")],
        bcc: vec![Mailbox::new("", "secret@example.org")],
        subject: "Grüße aus Köln".into(),
        html: "<div>Hallo <b>Lena</b>,</div><div>schön!</div>".into(),
        in_reply_to: Some("parent@example.org".into()),
        references: vec!["root@example.org".into(), "parent@example.org".into()],
        attachments: Vec::new(),
    }
}

#[test]
fn builds_headers() {
    let raw = build_message(&me(), &draft(), 1_795_167_720);
    let message = MessageParser::new().parse(&raw).unwrap();
    assert_eq!(message.subject(), Some("Grüße aus Köln"));
    assert_eq!(message.message_id(), Some("abc@example.org"));
    let from = message.from().unwrap().first().unwrap();
    assert_eq!(from.name(), Some("Max Berger"));
    assert_eq!(from.address(), Some("max@example.org"));
    assert_eq!(
        message.to().unwrap().first().unwrap().address(),
        Some("lena@example.org")
    );
    assert_eq!(
        message.cc().unwrap().first().unwrap().address(),
        Some("tom@example.org")
    );
    assert_eq!(
        message.bcc().unwrap().first().unwrap().address(),
        Some("secret@example.org")
    );
    assert_eq!(message.in_reply_to().as_text(), Some("parent@example.org"));
    assert_eq!(
        message.references().as_text_list().unwrap(),
        ["root@example.org", "parent@example.org"]
    );
    assert_eq!(message.date().unwrap().to_timestamp(), 1_795_167_720);
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Message-ID: <abc@example.org>"), "{text}");
    assert!(!text.contains("localhost"));
}

#[test]
fn html_gets_a_text_version() {
    let raw = build_message(&me(), &draft(), 0);
    let message = MessageParser::new().parse(&raw).unwrap();
    assert_eq!(
        message.content_type().unwrap().subtype(),
        Some("alternative")
    );
    assert_eq!(message.body_text(0).unwrap(), "Hallo Lena,\r\nschön!\r\n");
    assert!(message.body_html(0).unwrap().contains("<b>Lena</b>"));
}

#[test]
fn pictures_become_related_parts() {
    let mut draft = draft();
    let uri = data_uri("image/png", PNG);
    draft.html = format!(
        "<div>Logo: <img src=\"{uri}\"></div><div class=\"signature\"><img src=\"{uri}\" alt=\"Logo\"></div>"
    );
    draft.attachments.push(FileAttachment {
        name: "Rechnung.pdf".into(),
        mime_type: "application/pdf".into(),
        data: b"%PDF-1.4".to_vec(),
    });
    let raw = build_message(&me(), &draft, 0);
    let message = MessageParser::new().parse(&raw).unwrap();

    // mixed → alternative → (text, related → (html, image)), attachment
    assert_eq!(message.content_type().unwrap().subtype(), Some("mixed"));
    let types: Vec<String> = message
        .parts
        .iter()
        .map(|part| {
            let ct = part.content_type().unwrap();
            format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or_default())
        })
        .collect();
    assert_eq!(
        types,
        [
            "multipart/mixed",
            "multipart/alternative",
            "text/plain",
            "multipart/related",
            "text/html",
            "image/png",
            "application/pdf"
        ]
    );
    let image = message
        .parts
        .iter()
        .find(|part| part.content_id().is_some())
        .unwrap();
    assert_eq!(image.contents(), PNG);
    let cid = image.content_id().unwrap();
    assert_eq!(cid, "part1.abc@example.org");
    let html = message.body_html(0).unwrap();
    assert_eq!(html.matches(&format!("cid:{cid}")).count(), 2, "{html}");
    assert!(!html.contains("data:"));
    assert_eq!(crate::parse::parse_message(&raw).attachments.len(), 1);
    let attachment = message.parts.last().unwrap();
    assert_eq!(attachment.attachment_name(), Some("Rechnung.pdf"));
    assert!(matches!(attachment.body, PartType::Binary(_)));
    assert!(message.body_text(0).unwrap().contains("[Logo]"));
}

#[test]
fn bcc_does_not_leave_the_computer() {
    let raw = build_message(&me(), &draft(), 0);
    let wire = strip_bcc(&raw);
    let message = MessageParser::new().parse(&wire).unwrap();
    assert!(message.bcc().is_none());
    assert!(message.to().is_some());
    assert_eq!(message.body_text(0).unwrap(), "Hallo Lena,\r\nschön!\r\n");
    assert_eq!(
        draft().recipients(),
        ["lena@example.org", "tom@example.org", "secret@example.org"]
    );
    let folded =
        b"To: a@x.org\r\nBcc: one@x.org,\r\n two@x.org\r\nSubject: Hi\r\n\r\nBcc: stays\r\n";
    assert_eq!(
        strip_bcc(folded),
        b"To: a@x.org\r\nSubject: Hi\r\n\r\nBcc: stays\r\n".to_vec()
    );
}

#[test]
fn text_version_keeps_structure() {
    let html = "<div>Hi&nbsp;Anna,</div><div><br></div>\
<ul><li>one</li><li>two &amp; three</li></ul>\
<ol><li>first</li><li>second</li></ol>\
<p>See <a href=\"https://example.org/x\">the plan</a> or \
<a href=\"https://example.org\">https://example.org</a>.</p>\
<div class=\"signature\">-- <br>Max</div>\
<div>On Monday, Lena wrote:</div><blockquote type=\"cite\"><div>Hello</div>\
<blockquote><div>older</div></blockquote></blockquote><style>p{color:red}</style>";
    assert_eq!(
        html_to_text(html),
        "Hi Anna,\n\n- one\n- two & three\n1. first\n2. second\n\
See the plan <https://example.org/x> or https://example.org.\n\n-- \nMax\n\
On Monday, Lena wrote:\n> Hello\n> > older\n"
    );
}

#[test]
fn addresses_are_checked() {
    assert_eq!(
        parse_addresses("Lena Vogt <lena@example.org>, tom@example.org; \"Doe, Jane\" <j@x.de>")
            .unwrap(),
        [
            Mailbox::new("Lena Vogt", "lena@example.org"),
            Mailbox::new("", "tom@example.org"),
            Mailbox::new("Doe, Jane", "j@x.de"),
        ]
    );
    assert_eq!(
        parse_addresses("lena@example, ok@example.org, nope").unwrap_err(),
        ["lena@example", "nope"]
    );
    assert!(parse_addresses("  ").unwrap().is_empty());
}

#[test]
fn message_ids_are_unique_and_private() {
    let a = new_message_id("max@example.org");
    let b = new_message_id("max@example.org");
    assert_ne!(a, b);
    assert!(a.ends_with("@example.org"));
}

fn original() -> Original {
    read_original(
        b"From: Lena Vogt <lena@example.org>\r\n\
To: Max Berger <max@example.org>, Tom <tom@example.org>\r\n\
Cc: Kim <kim@example.org>, max@example.org\r\n\
Subject: Grillfest\r\n\
Message-ID: <m2@example.org>\r\n\
References: <m1@example.org>\r\n\
Date: Fri, 20 Nov 2026 10:42:00 +0100\r\n\
Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
--m\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n\
--r\r\nContent-Type: text/html\r\n\r\n<p onclick=\"x()\">Kommst du? <img src=\"cid:pic@x\"></p><script>bad()</script>\r\n\
--r\r\nContent-Type: image/png\r\nContent-ID: <pic@x>\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
--r--\r\n\
--m\r\nContent-Type: application/pdf; name=\"Plan.pdf\"\r\n\
Content-Disposition: attachment; filename=\"Plan.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n\
--m--\r\n",
    )
}

#[test]
fn reply_quotes_and_threads() {
    let draft = reply_draft(
        &original(),
        ReplyKind::Reply,
        "max@example.org",
        "<b>Max</b>",
        &texts(),
    );
    assert_eq!(draft.subject, "Re: Grillfest");
    assert_eq!(draft.to, [Mailbox::new("Lena Vogt", "lena@example.org")]);
    assert!(draft.cc.is_empty());
    assert_eq!(draft.in_reply_to.as_deref(), Some("m2@example.org"));
    assert_eq!(draft.references, ["m1@example.org", "m2@example.org"]);
    assert!(draft.attachments.is_empty());
    let signature = draft.html.find("-- <br><b>Max</b>").unwrap();
    let quote = draft.html.find("<blockquote type=\"cite\">").unwrap();
    assert!(signature < quote);
    assert!(draft.html.contains("On day 1795167720, Lena Vogt wrote:"));
    assert!(draft.html.contains("Kommst du?"));
    assert!(
        draft
            .html
            .contains("src=\"data:image/png;base64,aGVsbG8=\"")
    );
    assert!(!draft.html.contains("script"));
    assert!(!draft.html.contains("onclick"));

    // Replying again keeps one "Re:".
    assert_eq!(
        reply_subject("RE: Grillfest", ReplyKind::Reply),
        "RE: Grillfest"
    );
    assert_eq!(
        reply_subject("Grillfest", ReplyKind::Forward),
        "Fwd: Grillfest"
    );
}

#[test]
fn reply_all_leaves_out_myself() {
    let (to, cc) = reply_recipients(&original(), ReplyKind::ReplyAll, "MAX@example.org");
    assert_eq!(
        to,
        [
            Mailbox::new("Lena Vogt", "lena@example.org"),
            Mailbox::new("Tom", "tom@example.org"),
        ]
    );
    assert_eq!(cc, [Mailbox::new("Kim", "kim@example.org")]);
}

#[test]
fn reply_to_own_message_goes_to_its_recipients() {
    let mut own = original();
    own.from = vec![me()];
    own.to = vec![Mailbox::new("Lena Vogt", "lena@example.org")];
    let (to, _) = reply_recipients(&own, ReplyKind::Reply, "max@example.org");
    assert_eq!(to, [Mailbox::new("Lena Vogt", "lena@example.org")]);
    own.reply_to = Vec::new();
    let mut list = original();
    list.reply_to = vec![Mailbox::new("", "list@example.org")];
    let (to, _) = reply_recipients(&list, ReplyKind::Reply, "max@example.org");
    assert_eq!(to, [Mailbox::new("", "list@example.org")]);
}

#[test]
fn forward_keeps_attachments() {
    let draft = reply_draft(
        &original(),
        ReplyKind::Forward,
        "max@example.org",
        "",
        &texts(),
    );
    assert_eq!(draft.subject, "Fwd: Grillfest");
    assert!(draft.to.is_empty());
    assert_eq!(draft.in_reply_to, None);
    assert_eq!(draft.attachments.len(), 1);
    assert_eq!(draft.attachments[0].name, "Plan.pdf");
    assert_eq!(draft.attachments[0].data, b"%PDF");
    assert!(draft.html.contains("Forwarded message"));
    assert!(
        draft
            .html
            .contains("From: Lena Vogt &lt;lena@example.org&gt;")
    );
    assert!(!draft.html.contains("signature"));
}

#[test]
fn drafts_round_trip() {
    let mut original = draft();
    original.html = format!(
        "<div>Text</div><div><img src=\"{}\"></div>",
        data_uri("image/png", PNG)
    );
    original.attachments.push(FileAttachment {
        name: "notes.txt".into(),
        mime_type: "text/plain".into(),
        data: b"some notes".to_vec(),
    });
    let raw = build_message(&me(), &original, 0);
    let back = read_draft(&raw, "max@example.org");
    assert_eq!(back.message_id, original.message_id);
    assert_eq!(back.to, original.to);
    assert_eq!(back.cc, original.cc);
    assert_eq!(back.bcc, original.bcc);
    assert_eq!(back.subject, original.subject);
    assert_eq!(back.in_reply_to, original.in_reply_to);
    assert_eq!(back.references, original.references);
    assert_eq!(back.attachments, original.attachments);
    assert!(back.html.contains("<div>Text</div>"), "{}", back.html);
    assert!(back.html.contains(&data_uri("image/png", PNG)));
}

#[test]
fn new_drafts_carry_the_signature() {
    let draft = new_draft("max@example.org", "Viele Grüße<br>Max");
    assert!(
        draft
            .html
            .ends_with("<div class=\"signature\">-- <br>Viele Grüße<br>Max</div>")
    );
    assert!(draft.message_id.ends_with("@example.org"));
    assert!(!new_draft("max@example.org", " ").html.contains("signature"));
}
