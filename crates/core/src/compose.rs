// SPDX-License-Identifier: GPL-3.0-or-later

//! Writing messages: turning what the editor holds into a MIME message,
//! preparing replies and forwards, and reading a saved draft back.
//!
//! The editor works with HTML in which pictures are `data:` URIs. When a
//! message is built, every such picture becomes a part of a
//! `multipart/related` body and is referenced by `cid:`. Every message also
//! gets a plain-text version (`multipart/alternative`).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use base64::Engine;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId;
use mail_builder::mime::{BodyPart, MimePart};
use mail_parser::{Addr, HeaderValue, MessageParser};

use crate::html;
use crate::parse::{self, Body};

/// A sender or recipient.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mailbox {
    pub name: String,
    pub email: String,
}

impl Mailbox {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Mailbox {
        Mailbox {
            name: name.into(),
            email: email.into(),
        }
    }

    /// "Name <address>", or only the address without a name.
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            self.email.clone()
        } else {
            format!("{} <{}>", self.name, self.email)
        }
    }

    /// The name if there is one, otherwise the address.
    pub fn short(&self) -> &str {
        if self.name.is_empty() {
            &self.email
        } else {
            &self.name
        }
    }

    fn header(&self) -> Address<'_> {
        if self.name.is_empty() {
            Address::new_address(None::<&str>, self.email.as_str())
        } else {
            Address::new_address(Some(self.name.as_str()), self.email.as_str())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttachment {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// Everything the user is writing, in the form the editor uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    /// Stays the same for every saved version and the message finally
    /// sent, so a newer version can replace the older one. Without `<>`.
    pub message_id: String,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    pub subject: String,
    /// The inside of `<body>`, pictures as `data:` URIs.
    pub html: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<FileAttachment>,
}

impl Draft {
    /// Everyone who gets the message, for the SMTP envelope.
    pub fn recipients(&self) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for mailbox in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            if !all
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&mailbox.email))
            {
                all.push(mailbox.email.clone());
            }
        }
        all
    }

    pub fn has_recipients(&self) -> bool {
        !(self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty())
    }
}

/// A random, unique Message-ID in the domain of the sender. It does not
/// contain the name of the computer, unlike what many mail programs do.
pub fn new_message_id(sender: &str) -> String {
    let domain = sender
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| !domain.is_empty())
        .unwrap_or("localhost");
    format!("{:016x}{:016x}@{domain}", random(), random())
}

fn random() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish()
}

/// Builds the complete message. The Bcc header is included, as the copy in
/// "Sent" and drafts should keep it; [`strip_bcc`] removes it before the
/// message leaves the computer. `date` is a Unix timestamp.
pub fn build_message(from: &Mailbox, draft: &Draft, date: i64) -> Vec<u8> {
    let from = &clean_mailbox(from);
    let draft = &clean_headers(draft);
    let (html, images) = extract_images(&draft.html, &draft.message_id);
    let text = html_to_text(&html);
    let document = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body>{html}</body></html>"
    );

    let mut body = MimePart::new(
        "multipart/alternative",
        vec![
            MimePart::new("text/plain", text),
            related(MimePart::new("text/html", document), &images),
        ],
    );
    if !draft.attachments.is_empty() {
        let mut parts = vec![body];
        for attachment in &draft.attachments {
            parts.push(
                MimePart::new(
                    attachment.mime_type.as_str(),
                    BodyPart::Binary(attachment.data.as_slice().into()),
                )
                .attachment(attachment.name.as_str()),
            );
        }
        body = MimePart::new("multipart/mixed", parts);
    }

    let mut builder = MessageBuilder::new()
        .from(from.header())
        .message_id(draft.message_id.as_str())
        .date(Date::new(date))
        .subject(draft.subject.as_str())
        .header("User-Agent", mail_builder::headers::raw::Raw::new("E-Mail"));
    for (list, kind) in [(&draft.to, "To"), (&draft.cc, "Cc"), (&draft.bcc, "Bcc")] {
        if list.is_empty() {
            continue;
        }
        let addresses = Address::new_list(list.iter().map(Mailbox::header).collect());
        builder = match kind {
            "To" => builder.to(addresses),
            "Cc" => builder.cc(addresses),
            _ => builder.bcc(addresses),
        };
    }
    if let Some(parent) = &draft.in_reply_to {
        builder = builder.in_reply_to(parent.as_str());
    }
    if !draft.references.is_empty() {
        builder = builder.references(MessageId::new_list(
            draft.references.iter().map(String::as_str),
        ));
    }
    builder
        .body(body)
        .write_to_vec()
        .expect("writing to memory cannot fail")
}

/// Header texts without line breaks or other control characters, so that
/// pasted text cannot add headers of its own (such as a hidden Bcc).
fn clean_header(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn clean_id(id: &str) -> String {
    id.chars()
        .filter(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '<' | '>'))
        .collect()
}

fn clean_mailbox(mailbox: &Mailbox) -> Mailbox {
    Mailbox::new(clean_header(&mailbox.name), clean_id(&mailbox.email))
}

fn clean_headers(draft: &Draft) -> Draft {
    let list = |list: &[Mailbox]| list.iter().map(clean_mailbox).collect::<Vec<_>>();
    Draft {
        message_id: clean_id(&draft.message_id),
        to: list(&draft.to),
        cc: list(&draft.cc),
        bcc: list(&draft.bcc),
        subject: clean_header(&draft.subject),
        html: draft.html.clone(),
        in_reply_to: draft.in_reply_to.as_deref().map(clean_id),
        references: draft.references.iter().map(|id| clean_id(id)).collect(),
        attachments: draft
            .attachments
            .iter()
            .map(|attachment| FileAttachment {
                name: clean_header(&attachment.name).replace(['/', '\\'], "_"),
                mime_type: clean_id(&attachment.mime_type),
                data: attachment.data.clone(),
            })
            .collect(),
    }
}

fn related<'a>(html: MimePart<'a>, images: &'a [InlinePicture]) -> MimePart<'a> {
    if images.is_empty() {
        return html;
    }
    let mut parts = vec![html];
    for image in images {
        parts.push(
            MimePart::new(
                image.mime_type.as_str(),
                BodyPart::Binary(image.data.as_slice().into()),
            )
            .inline()
            .cid(image.content_id.as_str()),
        );
    }
    MimePart::new("multipart/related", parts)
}

/// Removes the Bcc header, which recipients must not see.
pub fn strip_bcc(raw: &[u8]) -> Vec<u8> {
    let end = header_end(raw);
    let (head, body) = raw.split_at(end);
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in head.split_inclusive(|&byte| byte == b'\n') {
        let continued = line.first().is_some_and(|&b| b == b' ' || b == b'\t');
        if continued {
            if !skipping {
                out.extend_from_slice(line);
            }
            continue;
        }
        skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(body);
    out
}

fn header_end(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 2)
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|pos| pos + 1)
        })
        .unwrap_or(raw.len())
}

/// A picture taken out of the editor HTML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlinePicture {
    pub content_id: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// Replaces every `data:` picture by a `cid:` reference. The same picture
/// used twice (a logo in the signature and in a quote) is attached once.
pub fn extract_images(html: &str, message_id: &str) -> (String, Vec<InlinePicture>) {
    let mut out = String::with_capacity(html.len());
    let mut images: Vec<InlinePicture> = Vec::new();
    let mut rest = html;
    while let Some(start) = find_data_src(rest) {
        let (before, after) = rest.split_at(start.value);
        out.push_str(before);
        let value_end = after.find(start.quote).unwrap_or(after.len());
        let value = &after[..value_end];
        match decode_data_uri(value) {
            Some((mime_type, data)) => {
                let content_id = match images.iter().find(|image| image.data == data) {
                    Some(image) => image.content_id.clone(),
                    None => {
                        let content_id = format!("part{}.{message_id}", images.len() + 1);
                        images.push(InlinePicture {
                            content_id: content_id.clone(),
                            mime_type,
                            data,
                        });
                        content_id
                    }
                };
                out.push_str("cid:");
                out.push_str(&content_id);
            }
            None => out.push_str(value),
        }
        rest = &after[value_end..];
    }
    out.push_str(rest);
    (out, images)
}

struct DataSrc {
    /// Byte position where the attribute value starts.
    value: usize,
    quote: char,
}

fn find_data_src(html: &str) -> Option<DataSrc> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("src=") {
        let at = from + pos + 4;
        let quote = lower[at..].chars().next()?;
        if (quote == '"' || quote == '\'') && lower[at + 1..].starts_with("data:") {
            return Some(DataSrc {
                value: at + 1,
                quote,
            });
        }
        from = at;
    }
    None
}

fn decode_data_uri(value: &str) -> Option<(String, Vec<u8>)> {
    let rest = value.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime_type = meta.strip_suffix(";base64")?.trim().to_ascii_lowercase();
    if !mime_type.starts_with("image/") {
        return None;
    }
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let data = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    Some((mime_type, data))
}

/// A picture as `data:` URI for the editor.
pub fn data_uri(mime_type: &str, data: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

/// The plain-text version of editor HTML: paragraphs and line breaks are
/// kept, lists get dashes, quotes get "> ", links show their address.
pub fn html_to_text(html: &str) -> String {
    let mut writer = TextWriter::default();
    let mut skip_depth = 0usize;
    let mut links: Vec<Option<String>> = Vec::new();
    let mut lists: Vec<Option<usize>> = Vec::new();
    for token in tokenize(html) {
        match token {
            Token::Text(text) => {
                if skip_depth == 0 {
                    writer.text(&decode_entities(&text));
                }
            }
            Token::Open { name, attributes } => {
                if matches!(name.as_str(), "style" | "script" | "head" | "title") {
                    skip_depth += 1;
                    continue;
                }
                match name.as_str() {
                    "br" => writer.line_break(),
                    "blockquote" => {
                        writer.block();
                        writer.depth += 1;
                    }
                    "ul" => {
                        writer.block();
                        lists.push(None);
                    }
                    "ol" => {
                        writer.block();
                        lists.push(Some(0));
                    }
                    "li" => {
                        writer.block();
                        let marker = match lists.last_mut() {
                            Some(Some(count)) => {
                                *count += 1;
                                format!("{count}. ")
                            }
                            _ => "- ".to_owned(),
                        };
                        writer.raw(&marker);
                    }
                    "a" => links.push(attribute(&attributes, "href")),
                    "img" => {
                        if let Some(alt) = attribute(&attributes, "alt")
                            && !alt.trim().is_empty()
                        {
                            writer.text(&format!("[{}]", decode_entities(alt.trim())));
                        }
                    }
                    name if is_block(name) => writer.block(),
                    _ => {}
                }
            }
            Token::Close { name } => {
                if matches!(name.as_str(), "style" | "script" | "head" | "title") {
                    skip_depth = skip_depth.saturating_sub(1);
                    continue;
                }
                match name.as_str() {
                    "blockquote" => {
                        writer.block();
                        writer.depth = writer.depth.saturating_sub(1);
                    }
                    "ul" | "ol" => {
                        lists.pop();
                        writer.block();
                    }
                    "a" => {
                        if let Some(Some(href)) = links.pop() {
                            writer.link_target(&decode_entities(&href));
                        }
                    }
                    "p" => {
                        writer.block();
                        writer.blank_line();
                    }
                    name if is_block(name) => writer.block(),
                    _ => {}
                }
            }
        }
    }
    writer.finish()
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "div"
            | "p"
            | "li"
            | "tr"
            | "table"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "hr"
            | "section"
            | "article"
            | "header"
            | "footer"
    )
}

#[derive(Default)]
struct TextWriter {
    lines: Vec<(usize, String)>,
    line: String,
    line_depth: usize,
    depth: usize,
    /// Text of the current link, to leave out the address when it repeats it.
    last_text: String,
    pending_space: bool,
}

impl TextWriter {
    fn text(&mut self, text: &str) {
        for (index, word) in text.split(|c: char| c.is_whitespace()).enumerate() {
            if index > 0 {
                self.pending_space = true;
            }
            if word.is_empty() {
                continue;
            }
            if self.line.is_empty() {
                self.line_depth = self.depth;
            } else if self.pending_space && !self.line.ends_with(' ') {
                self.line.push(' ');
            }
            self.pending_space = false;
            self.line.push_str(word);
            self.last_text = word.to_owned();
        }
        if text.ends_with(char::is_whitespace) && !self.line.is_empty() {
            self.pending_space = true;
        }
    }

    fn raw(&mut self, text: &str) {
        if self.line.is_empty() {
            self.line_depth = self.depth;
        }
        self.line.push_str(text);
        self.pending_space = false;
    }

    fn link_target(&mut self, href: &str) {
        let target = href.strip_prefix("mailto:").unwrap_or(href);
        if target.is_empty() || self.line.contains(target) || self.last_text == target {
            return;
        }
        self.raw(&format!(" <{target}>"));
    }

    fn line_break(&mut self) {
        if self.line == "--" && self.pending_space {
            self.line.push(' ');
        }
        if self.line.is_empty() {
            self.line_depth = self.depth;
        }
        self.lines
            .push((self.line_depth, std::mem::take(&mut self.line)));
        self.pending_space = false;
    }

    /// Ends the current line, if anything is on it.
    fn block(&mut self) {
        if !self.line.trim().is_empty() {
            self.line_break();
        } else {
            self.line.clear();
        }
        self.pending_space = false;
    }

    fn blank_line(&mut self) {
        if self
            .lines
            .last()
            .is_some_and(|(_, line)| !line.trim().is_empty())
        {
            self.lines.push((self.depth, String::new()));
        }
    }

    fn finish(mut self) -> String {
        self.block();
        let mut out = String::new();
        let mut blank_run = 0;
        for (depth, line) in &self.lines {
            // "-- " opens a signature and keeps its space by convention.
            let line = if line == "-- " {
                line.as_str()
            } else {
                line.trim_end()
            };
            if line.is_empty() && *depth == 0 {
                blank_run += 1;
                if blank_run > 1 {
                    continue;
                }
            } else {
                blank_run = 0;
            }
            let prefix = "> ".repeat(*depth);
            let full = format!("{prefix}{line}");
            out.push_str(if line.is_empty() {
                full.trim_end()
            } else {
                &full
            });
            out.push('\n');
        }
        out.trim_matches('\n').to_owned() + "\n"
    }
}

enum Token {
    Text(String),
    Open { name: String, attributes: String },
    Close { name: String },
}

fn tokenize(html: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find('<') {
            Some(0) => {
                if let Some(after) = rest.strip_prefix("<!--") {
                    rest = after.find("-->").map_or("", |end| &after[end + 3..]);
                    continue;
                }
                let Some(end) = tag_end(rest) else {
                    tokens.push(Token::Text(rest.to_owned()));
                    break;
                };
                let inner = &rest[1..end];
                rest = &rest[end + 1..];
                if let Some(name) = inner.strip_prefix('/') {
                    tokens.push(Token::Close {
                        name: name.trim().to_ascii_lowercase(),
                    });
                } else if !inner.starts_with('!') && !inner.starts_with('?') {
                    let inner = inner.trim_end_matches('/');
                    let (name, attributes) = inner
                        .split_once(|c: char| c.is_whitespace())
                        .unwrap_or((inner, ""));
                    tokens.push(Token::Open {
                        name: name.to_ascii_lowercase(),
                        attributes: attributes.to_owned(),
                    });
                }
            }
            Some(pos) => {
                tokens.push(Token::Text(rest[..pos].to_owned()));
                rest = &rest[pos..];
            }
            None => {
                tokens.push(Token::Text(rest.to_owned()));
                break;
            }
        }
    }
    tokens
}

/// The closing `>` of a tag, ignoring any inside quoted attribute values.
fn tag_end(html: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, c) in html.char_indices().skip(1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(index),
            None => {}
        }
    }
    None
}

fn attribute(attributes: &str, wanted: &str) -> Option<String> {
    let mut rest = attributes;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let name_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let end = after[1..].find(q).map_or(after.len(), |end| end + 1);
                    rest = after.get(end + 1..).unwrap_or("");
                    after[1..end].to_owned()
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    rest = &after[end..];
                    after[..end].to_owned()
                }
            }
        } else {
            String::new()
        };
        if name == wanted {
            return Some(value);
        }
    }
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escapes text for HTML and keeps its line breaks.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.trim_end().lines().enumerate() {
        if index > 0 {
            out.push_str("<br>");
        }
        out.push_str(&escape(line));
    }
    out
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Splits what the user typed into addresses. Accepts "Name <a@b>",
/// plain addresses and lists separated by commas or semicolons. Returns
/// the parts that are no valid address as error.
pub fn parse_addresses(input: &str) -> Result<Vec<Mailbox>, Vec<String>> {
    let mut mailboxes = Vec::new();
    let mut invalid = Vec::new();
    for part in split_addresses(input) {
        match parse_mailbox(&part) {
            Some(mailbox) => mailboxes.push(mailbox),
            None => invalid.push(part),
        }
    }
    if invalid.is_empty() {
        Ok(mailboxes)
    } else {
        Err(invalid)
    }
}

fn split_addresses(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angle = false;
    for c in input.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

pub fn parse_mailbox(input: &str) -> Option<Mailbox> {
    let input = input.trim();
    let (name, email) = match (input.rfind('<'), input.ends_with('>')) {
        (Some(open), true) => (
            input[..open].trim().trim_matches('"').trim(),
            input[open + 1..input.len() - 1].trim(),
        ),
        _ => ("", input.trim_start_matches("mailto:")),
    };
    is_valid_address(email).then(|| Mailbox::new(name, email))
}

/// A simple check that catches typing mistakes; the server decides the rest.
pub fn is_valid_address(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';'))
}

/// The parts of a received message needed to answer or forward it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Original {
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub from: Vec<Mailbox>,
    pub reply_to: Vec<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    pub date: Option<i64>,
    /// Cleaned HTML of the body with pictures as `data:` URIs.
    pub html: String,
    pub attachments: Vec<FileAttachment>,
}

pub fn read_original(raw: &[u8]) -> Original {
    let Some(message) = MessageParser::new().parse(raw) else {
        return Original::default();
    };
    let parsed = parse::parse_message(raw);
    let html = match &parsed.body {
        Body::Html(source) => html::clean_fragment(source, &parsed.inline_images),
        Body::Text(text) => text_to_html(text),
        Body::Empty => String::new(),
    };
    let attachments = parsed
        .attachments
        .iter()
        .filter_map(|attachment| {
            Some(FileAttachment {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data: parse::attachment_data(raw, attachment.part)?,
            })
        })
        .collect();
    Original {
        message_id: parsed.header.message_id.clone(),
        references: parsed.header.references.clone(),
        subject: message.subject().unwrap_or_default().trim().to_owned(),
        from: mailboxes(message.from()),
        reply_to: mailboxes(message.reply_to()),
        to: mailboxes(message.to()),
        cc: mailboxes(message.cc()),
        bcc: mailboxes(message.bcc()),
        date: parsed.header.date,
        html,
        attachments,
    }
}

fn mailboxes(value: Option<&mail_parser::Address<'_>>) -> Vec<Mailbox> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .iter()
        .filter_map(|addr: &Addr<'_>| {
            let email = addr.address()?.trim().to_owned();
            Some(Mailbox::new(
                addr.name().unwrap_or_default().trim().to_owned(),
                email,
            ))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

/// Texts that go into replies and forwards, in the language of the app.
pub struct QuoteTexts {
    /// "On {date}, {name} wrote:"
    pub wrote: String,
    /// "Forwarded message"
    pub forwarded: String,
    pub from: String,
    pub date: String,
    pub subject: String,
    pub to: String,
    /// Formats the date of the original message.
    pub format_date: Box<dyn Fn(i64) -> String>,
}

/// Recipients of a reply. Answers to an own message go to its recipients.
pub fn reply_recipients(
    original: &Original,
    kind: ReplyKind,
    own: &str,
) -> (Vec<Mailbox>, Vec<Mailbox>) {
    let is_own = |m: &Mailbox| m.email.eq_ignore_ascii_case(own);
    let from_me = original.from.iter().any(is_own);
    let mut to: Vec<Mailbox> = if from_me {
        original.to.clone()
    } else if !original.reply_to.is_empty() {
        original.reply_to.clone()
    } else {
        original.from.clone()
    };
    let mut cc = Vec::new();
    if kind == ReplyKind::ReplyAll {
        if !from_me {
            to.extend(original.to.iter().cloned());
        }
        cc = original.cc.clone();
    }
    let mut seen: Vec<String> = Vec::new();
    let mut keep = |m: &Mailbox| {
        let key = m.email.to_ascii_lowercase();
        if is_own(m) || seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    };
    to.retain(&mut keep);
    cc.retain(&mut keep);
    if to.is_empty() && from_me {
        // A note to oneself: answer oneself.
        to = original.from.clone();
    }
    (to, cc)
}

pub fn reply_subject(subject: &str, kind: ReplyKind) -> String {
    let subject = subject.trim();
    let (prefix, known): (&str, &[&str]) = match kind {
        ReplyKind::Forward => ("Fwd: ", &["fwd:", "fw:"]),
        _ => ("Re: ", &["re:"]),
    };
    let lower = subject.to_lowercase();
    if known.iter().any(|known| lower.starts_with(known)) {
        subject.to_owned()
    } else {
        format!("{prefix}{subject}")
    }
}

/// The signature as it is placed below new text.
pub fn signature_block(signature: &str) -> String {
    if signature.trim().is_empty() {
        String::new()
    } else {
        format!("<div class=\"signature\">-- <br>{signature}</div>")
    }
}

/// A new, empty message with the signature.
pub fn new_draft(sender: &str, signature: &str) -> Draft {
    Draft {
        message_id: new_message_id(sender),
        html: format!(
            "<div><br></div><div><br></div>{}",
            signature_block(signature)
        ),
        ..Draft::default()
    }
}

/// A reply or forward of `original`, with the quote below the signature.
pub fn reply_draft(
    original: &Original,
    kind: ReplyKind,
    own: &str,
    signature: &str,
    texts: &QuoteTexts,
) -> Draft {
    let mut draft = Draft {
        message_id: new_message_id(own),
        subject: reply_subject(&original.subject, kind),
        ..Draft::default()
    };
    let mut references = original.references.clone();
    if let Some(id) = &original.message_id
        && !references.contains(id)
    {
        references.push(id.clone());
    }
    let date = original
        .date
        .map(|date| (texts.format_date)(date))
        .unwrap_or_default();
    let sender = original
        .from
        .first()
        .map(|m| m.short().to_owned())
        .unwrap_or_default();
    let quote = match kind {
        ReplyKind::Forward => {
            draft.attachments = original.attachments.clone();
            draft.references = references;
            let list = |list: &[Mailbox]| {
                list.iter()
                    .map(Mailbox::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut header = format!("---------- {} ----------<br>", escape(&texts.forwarded));
            for (label, value) in [
                (&texts.from, list(&original.from)),
                (&texts.date, date),
                (&texts.subject, original.subject.clone()),
                (&texts.to, list(&original.to)),
            ] {
                if !value.is_empty() {
                    header.push_str(&format!("{}: {}<br>", escape(label), escape(&value)));
                }
            }
            format!("<div>{header}</div><br><div>{}</div>", original.html)
        }
        _ => {
            let (to, cc) = reply_recipients(original, kind, own);
            draft.to = to;
            draft.cc = cc;
            draft.in_reply_to = original.message_id.clone();
            draft.references = references;
            let line = texts
                .wrote
                .replace("{date}", &date)
                .replace("{name}", &sender);
            format!(
                "<div>{}</div><blockquote type=\"cite\">{}</blockquote>",
                escape(&line),
                original.html
            )
        }
    };
    draft.html = format!(
        "<div><br></div><div><br></div>{}<br>{quote}",
        signature_block(signature)
    );
    draft
}

/// Reads a saved draft back so it can be edited again.
pub fn read_draft(raw: &[u8], sender: &str) -> Draft {
    let original = read_original(raw);
    let in_reply_to =
        MessageParser::new()
            .parse_headers(raw)
            .and_then(|message| match message.in_reply_to() {
                HeaderValue::Text(id) => Some(id.to_string()),
                HeaderValue::TextList(ids) => ids.first().map(|id| id.to_string()),
                _ => None,
            });
    Draft {
        message_id: original
            .message_id
            .clone()
            .unwrap_or_else(|| new_message_id(sender)),
        to: original.to,
        cc: original.cc,
        bcc: original.bcc,
        subject: original.subject,
        html: original.html,
        in_reply_to,
        references: original.references,
        attachments: original.attachments,
    }
}

/// The type of a file from its name, for attachments and pictures.
pub fn mime_type_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ics" => "text/calendar",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests;
