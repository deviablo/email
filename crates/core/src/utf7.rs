// SPDX-License-Identifier: GPL-3.0-or-later

//! IMAP mailbox names use a modified UTF-7 (RFC 3501, section 5.1.3).

use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};

fn engine() -> GeneralPurpose {
    let alphabet =
        Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,")
            .expect("valid alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    )
}

/// Decodes a mailbox name. Invalid sequences are kept as they are, because
/// showing a slightly odd name is better than hiding the folder.
pub fn decode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let encoded = &after[..end];
        if encoded.is_empty() {
            out.push('&');
        } else {
            match decode_utf16_base64(encoded) {
                Some(text) => out.push_str(&text),
                None => out.push_str(&rest[start..start + end + 2]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_utf16_base64(encoded: &str) -> Option<String> {
    let bytes = engine().decode(encoded).ok()?;
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn plain_names_stay_unchanged() {
        assert_eq!(decode("INBOX"), "INBOX");
        assert_eq!(decode("Projects/2026"), "Projects/2026");
    }

    #[test]
    fn decodes_umlauts() {
        assert_eq!(decode("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode("Gel&APY-scht"), "Gelöscht");
    }

    #[test]
    fn decodes_escaped_ampersand() {
        assert_eq!(decode("Tom &- Jerry"), "Tom & Jerry");
    }

    #[test]
    fn decodes_non_latin() {
        assert_eq!(decode("&ZeVnLIqe-"), "日本語");
    }

    #[test]
    fn keeps_broken_sequences() {
        assert_eq!(decode("Broken&"), "Broken&");
        assert_eq!(decode("Bad&A-x"), "Bad&A-x");
    }
}
