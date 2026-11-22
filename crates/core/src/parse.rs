// SPDX-License-Identifier: GPL-3.0-or-later

use mail_parser::{HeaderValue, Message, MessageParser, MimeHeaders};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderInfo {
    pub subject: String,
    pub from_name: String,
    pub from_address: String,
    pub to: String,
    /// Unix timestamp in seconds, if the message carries a readable date.
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// A `multipart/mixed` message almost always carries attachments. Used
    /// until the whole message is loaded and the real answer is known.
    pub likely_attachment: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Html(String),
    Text(String),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    pub content_id: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// Position among the parts of the message, see [`attachment_data`].
    pub part: usize,
    pub name: String,
    pub mime_type: String,
    pub size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMessage {
    pub header: HeaderInfo,
    pub body: Body,
    pub preview: String,
    pub inline_images: Vec<InlineImage>,
    pub attachments: Vec<Attachment>,
}

const PREVIEW_LENGTH: usize = 200;

/// Reads the header block fetched with `BODY.PEEK[HEADER]`.
pub fn parse_header(raw: &[u8]) -> HeaderInfo {
    match MessageParser::new().parse_headers(raw) {
        Some(message) => header_of(&message),
        None => HeaderInfo::default(),
    }
}

pub fn parse_message(raw: &[u8]) -> ParsedMessage {
    let Some(message) = MessageParser::new().parse(raw) else {
        return ParsedMessage {
            header: HeaderInfo::default(),
            body: Body::Text(String::from_utf8_lossy(raw).into_owned()),
            preview: String::new(),
            inline_images: Vec::new(),
            attachments: Vec::new(),
        };
    };

    let html = message
        .html_body
        .iter()
        .filter_map(|id| message.parts.get(*id as usize))
        .find(|part| part.is_text_html())
        .and_then(|part| part.text_contents())
        .map(str::to_owned);
    let body = match html {
        Some(html) => Body::Html(html),
        None => match message.body_text(0) {
            Some(text) => Body::Text(text.into_owned()),
            None => Body::Empty,
        },
    };

    let inline_images = message
        .parts
        .iter()
        .filter_map(|part| {
            let content_id = part.content_id()?;
            let content_type = part.content_type()?;
            if !content_type.ctype().eq_ignore_ascii_case("image") {
                return None;
            }
            Some(InlineImage {
                content_id: content_id
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned(),
                mime_type: format!(
                    "image/{}",
                    content_type.subtype().unwrap_or("png").to_ascii_lowercase()
                ),
                data: part.contents().to_vec(),
            })
        })
        .collect();

    let preview = message
        .body_preview(PREVIEW_LENGTH)
        .map(|text| collapse_whitespace(&text))
        .unwrap_or_default();

    ParsedMessage {
        header: header_of(&message),
        body,
        preview,
        inline_images,
        attachments: attachments_of(&message),
    }
}

fn attachments_of(message: &Message<'_>) -> Vec<Attachment> {
    message
        .attachments
        .iter()
        .filter_map(|&index| {
            let index = index as usize;
            let part = message.parts.get(index)?;
            let content_type = part.content_type();
            let mime_type = content_type.map_or_else(
                || "application/octet-stream".to_owned(),
                |ct| match ct.subtype() {
                    Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
                    None => ct.ctype().to_ascii_lowercase(),
                },
            );
            let explicit = part
                .content_disposition()
                .is_some_and(|disposition| disposition.is_attachment());
            // Pictures shown inside the text are not attachments.
            if !explicit && part.content_id().is_some() && mime_type.starts_with("image/") {
                return None;
            }
            let name = part
                .attachment_name()
                .map(str::to_owned)
                .or_else(|| {
                    part.message()
                        .and_then(|inner| inner.subject())
                        .map(|subject| format!("{subject}.eml"))
                })
                .unwrap_or_else(|| match mime_type.as_str() {
                    "message/rfc822" => "message.eml".to_owned(),
                    _ => "attachment".to_owned(),
                });
            let size = match part.message() {
                Some(inner) => inner.raw_message().len(),
                None => part.contents().len(),
            };
            Some(Attachment {
                part: index,
                name: sanitize_file_name(&name),
                mime_type,
                size,
            })
        })
        .collect()
}

/// The decoded content of an attachment found by [`parse_message`].
pub fn attachment_data(raw: &[u8], part: usize) -> Option<Vec<u8>> {
    let message = MessageParser::new().parse(raw)?;
    let part = message.parts.get(part)?;
    Some(match part.message() {
        Some(inner) => inner.raw_message().to_vec(),
        None => part.contents().to_vec(),
    })
}

/// Keeps a file name from escaping the folder it is saved in.
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else {
        cleaned
    }
}

/// Plain text of a body, used for the search index.
pub fn searchable_text(body: &Body) -> String {
    match body {
        Body::Html(html) => mail_parser::decoders::html::html_to_text(html),
        Body::Text(text) => text.clone(),
        Body::Empty => String::new(),
    }
}

fn header_of(message: &Message<'_>) -> HeaderInfo {
    let sender = message.from().and_then(|list| list.first());
    let to = message
        .to()
        .map(|list| {
            list.iter()
                .map(|addr| {
                    addr.name()
                        .or(addr.address())
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    HeaderInfo {
        subject: message
            .subject()
            .map(collapse_whitespace)
            .unwrap_or_default(),
        from_name: sender
            .and_then(|addr| addr.name())
            .map(collapse_whitespace)
            .unwrap_or_default(),
        from_address: sender
            .and_then(|addr| addr.address())
            .unwrap_or_default()
            .to_owned(),
        to,
        date: message
            .date()
            .filter(|date| date.is_valid())
            .map(|date| date.to_timestamp()),
        message_id: message.message_id().map(str::to_owned),
        in_reply_to: id_list(message.in_reply_to()),
        references: id_list(message.references()),
        likely_attachment: message.content_type().is_some_and(|ct| {
            ct.ctype().eq_ignore_ascii_case("multipart")
                && ct
                    .subtype()
                    .is_some_and(|sub| sub.eq_ignore_ascii_case("mixed"))
        }),
    }
}

fn id_list(value: &HeaderValue<'_>) -> Vec<String> {
    value
        .as_text_list()
        .map(|ids| ids.iter().map(|id| id.to_string()).collect())
        .unwrap_or_default()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &[u8] = b"From: =?UTF-8?Q?J=C3=BCrgen_M=C3=BCller?= <juergen@example.org>\r\n\
To: Anna <anna@example.org>, bob@example.org\r\n\
Subject: =?ISO-8859-1?Q?Gr=FC=DFe_aus?=\r\n =?ISO-8859-1?Q?_K=F6ln?=\r\n\
Date: Fri, 20 Nov 2026 10:42:00 +0100\r\n\
Message-ID: <abc@example.org>\r\n\r\n";

    #[test]
    fn decodes_encoded_headers() {
        let header = parse_header(HEADER);
        assert_eq!(header.subject, "Grüße aus Köln");
        assert_eq!(header.from_name, "Jürgen Müller");
        assert_eq!(header.from_address, "juergen@example.org");
        assert_eq!(header.to, "Anna, bob@example.org");
        assert_eq!(header.message_id.as_deref(), Some("abc@example.org"));
        assert_eq!(header.date, Some(1_795_167_720));
        assert!(header.references.is_empty());
        assert!(!header.likely_attachment);
    }

    #[test]
    fn reads_reply_headers() {
        let header = parse_header(
            b"Subject: Re: Plan\r\nMessage-ID: <c@x>\r\nIn-Reply-To: <b@x>\r\n\
References: <a@x>\r\n <b@x>\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
        );
        assert_eq!(header.in_reply_to, ["b@x"]);
        assert_eq!(header.references, ["a@x", "b@x"]);
        assert!(header.likely_attachment);
    }

    #[test]
    fn lists_attachments_but_not_inline_images() {
        let raw = b"From: a@example.org\r\n\
Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
--m\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n\
--r\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
--r\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
--r--\r\n\
--m\r\nContent-Type: application/pdf; name=\"../Rechnung.pdf\"\r\n\
Content-Disposition: attachment; filename=\"../Rechnung.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n\
--m--\r\n";
        let message = parse_message(raw);
        assert_eq!(message.inline_images.len(), 1);
        assert_eq!(message.attachments.len(), 1);
        let attachment = &message.attachments[0];
        assert_eq!(attachment.name, "_Rechnung.pdf");
        assert_eq!(attachment.mime_type, "application/pdf");
        assert_eq!(attachment.size, 4);
        assert_eq!(attachment_data(raw, attachment.part).unwrap(), b"%PDF");
    }

    #[test]
    fn missing_fields_are_empty() {
        let header = parse_header(b"X-Nothing: here\r\n\r\n");
        assert_eq!(header, HeaderInfo::default());
    }

    #[test]
    fn prefers_html_part() {
        let raw = b"From: a@example.org\r\n\
Subject: Both\r\n\
Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello plain\r\n\
--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>html</b></p>\r\n\
--b--\r\n";
        let message = parse_message(raw);
        match message.body {
            Body::Html(html) => assert!(html.contains("<b>html</b>")),
            other => panic!("expected html, got {other:?}"),
        }
        assert_eq!(message.preview, "Hello plain");
    }

    #[test]
    fn plain_text_stays_text() {
        let raw = b"From: a@example.org\r\nSubject: Plain\r\n\
Content-Type: text/plain; charset=iso-8859-1\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\r\nSch=F6ne Gr=FC=DFe\r\n";
        let message = parse_message(raw);
        assert_eq!(message.body, Body::Text("Schöne Grüße\r\n".into()));
    }

    #[test]
    fn collects_inline_images() {
        let raw = b"From: a@example.org\r\n\
Content-Type: multipart/related; boundary=\"r\"\r\n\r\n\
--r\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
--r\r\nContent-Type: image/PNG\r\nContent-ID: <logo@x>\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
--r--\r\n";
        let message = parse_message(raw);
        assert_eq!(
            message.inline_images,
            vec![InlineImage {
                content_id: "logo@x".into(),
                mime_type: "image/png".into(),
                data: b"hello".to_vec(),
            }]
        );
    }
}
