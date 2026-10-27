//! Turns the bytes of a Markdown file (or stdin) into the text the parser sees: decoded
//! from whatever encoding it is in, without byte order marks, with `\n` line endings.

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

pub struct Decoded {
    pub text: String,
    /// Name of the encoding when it was not UTF-8, e.g. `UTF-16LE` or `windows-1252`.
    pub encoding: Option<&'static str>,
}

/// A byte order mark picks the encoding (UTF-8, UTF-16LE/BE). Without one the text is
/// UTF-8 if it is valid UTF-8, else the legacy 8-bit encoding: the system ANSI code page
/// on Windows, Windows-1252 elsewhere.
pub fn decode(bytes: Vec<u8>) -> Decoded {
    decode_with(bytes, legacy_encoding())
}

fn decode_with(bytes: Vec<u8>, legacy: &'static Encoding) -> Decoded {
    let (text, encoding) = if let Some((enc, bom_len)) = Encoding::for_bom(&bytes) {
        let (text, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        (text.into_owned(), (enc != UTF_8).then(|| enc.name()))
    } else {
        match String::from_utf8(bytes) {
            Ok(text) => (text, None),
            Err(e) if mostly_utf8(e.as_bytes()) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), None),
            Err(e) => {
                let (text, _) = legacy.decode_without_bom_handling(e.as_bytes());
                (text.into_owned(), Some(legacy.name()))
            }
        }
    };
    Decoded { text: normalize(text), encoding }
}

/// Invalid UTF-8 that is still UTF-8 for all practical purposes: a multi-byte character
/// cut off at the end (a file read while it is being written), or a few stray bytes (say
/// one character pasted from a cp1252 source) among more valid multi-byte characters.
/// Decoding those as the legacy code page would garble every correct character. Text in
/// a legacy encoding, Shift_JIS and GBK included, only forms a valid multi-byte UTF-8
/// sequence now and then. A single trailing lead byte after plain ASCII is ambiguous
/// (`é` in cp1252 ends many words), so it only counts as cut off after other UTF-8.
fn mostly_utf8(bytes: &[u8]) -> bool {
    let (mut valid, mut invalid) = (0usize, 0usize);
    let mut chunks = bytes.utf8_chunks().peekable();
    while let Some(chunk) = chunks.next() {
        valid += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
        let bad = chunk.invalid();
        let cut_off = chunks.peek().is_none() && (bad.len() >= 2 || valid > 0) && std::str::from_utf8(bad).is_err_and(|e| e.error_len().is_none());
        if !bad.is_empty() && !cut_off {
            invalid += 1;
        }
    }
    invalid == 0 || valid > invalid
}

/// CRLF and lone CR become LF: pulldown-cmark keeps the CR in code spans (as an extra
/// space) and math. Byte order marks at the start of any line also go, as files joined
/// with `copy /b a.md+b.md` or `type a.md b.md` keep one from each part, and a heading
/// behind one is not a heading.
fn normalize(mut s: String) -> String {
    if s.contains('\r') {
        s = s.replace("\r\n", "\n").replace('\r', "\n");
    }
    if s.contains('\u{FEFF}') {
        s = s.strip_prefix('\u{FEFF}').unwrap_or(&s).replace("\n\u{FEFF}", "\n");
    }
    s
}

#[cfg(windows)]
fn legacy_encoding() -> &'static Encoding {
    encoding_for_code_page(crate::win::ansi_code_page()).unwrap_or(WINDOWS_1252)
}

#[cfg(not(windows))]
fn legacy_encoding() -> &'static Encoding {
    WINDOWS_1252
}

/// The encoding of a Windows ANSI code page. `None` for 65001 (the "Use Unicode UTF-8 for
/// worldwide language support" setting), where text that is not UTF-8 has no better guess.
#[cfg_attr(not(windows), allow(dead_code))]
fn encoding_for_code_page(cp: u32) -> Option<&'static Encoding> {
    use encoding_rs::*;
    Some(match cp {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8]) -> (String, Option<&'static str>) {
        let d = decode_with(bytes.to_vec(), WINDOWS_1252);
        (d.text, d.encoding)
    }

    fn utf16(s: &str, le: bool) -> Vec<u8> {
        let mut out = if le { vec![0xFF, 0xFE] } else { vec![0xFE, 0xFF] };
        for u in s.encode_utf16() {
            out.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    #[test]
    fn utf8_passes_through() {
        assert_eq!(dec("# Café\n".as_bytes()), ("# Café\n".into(), None));
    }

    #[test]
    fn utf8_bom_is_stripped() {
        assert_eq!(dec(b"\xEF\xBB\xBF---\ntitle: x\n---\n"), ("---\ntitle: x\n---\n".into(), None));
    }

    #[test]
    fn utf16_with_bom_is_decoded() {
        assert_eq!(dec(&utf16("# Tïtle\r\n\r\nbody\r\n", true)), ("# Tïtle\n\nbody\n".into(), Some("UTF-16LE")));
        assert_eq!(dec(&utf16("# Tïtle\n", false)), ("# Tïtle\n".into(), Some("UTF-16BE")));
    }

    #[test]
    fn invalid_utf8_falls_back_to_the_legacy_code_page() {
        assert_eq!(dec(b"caf\xE9 \x93quoted\x94 \x80"), ("café \u{201C}quoted\u{201D} €".into(), Some("windows-1252")));
        let d = decode_with(b"\x82\xA0".to_vec(), encoding_for_code_page(932).unwrap());
        assert_eq!((d.text.as_str(), d.encoding), ("あ", Some("Shift_JIS")));
    }

    #[test]
    fn stray_bytes_in_utf8_text_cost_one_character() {
        let bytes = ["café 日本 ".as_bytes(), b"\xA0", b" x"].concat();
        assert_eq!(dec(&bytes), ("café 日本 \u{FFFD} x".into(), None));
    }

    #[test]
    fn utf8_cut_off_mid_character_stays_utf8() {
        // A reload that reads the file while the editor is still writing it.
        assert_eq!(dec(b"# Notes \xE6\x97"), ("# Notes \u{FFFD}".into(), None));
        assert_eq!(dec(b"caf\xC3\xA9 \xE6"), ("café \u{FFFD}".into(), None));
        // A lone high byte ending plain text is a legacy character, though.
        assert_eq!(dec(b"R\xE9sum\xE9"), ("Résumé".into(), Some("windows-1252")));
    }

    #[test]
    fn legacy_multibyte_text_is_not_taken_for_utf8() {
        let samples = [
            (932, "# 日本語の文書\n\n漢字とひらがなを含む文章です。カタカナも書きます。\n"),
            (936, "# 中文文档\n\n这是一个包含汉字的段落，用来测试编码。\n"),
        ];
        for (cp, text) in samples {
            let enc = encoding_for_code_page(cp).unwrap();
            let (bytes, _, _) = enc.encode(text);
            assert!(std::str::from_utf8(&bytes).is_err());
            let d = decode_with(bytes.into_owned(), enc);
            assert_eq!((d.text.as_str(), d.encoding), (text, Some(enc.name())));
        }
    }

    #[test]
    fn line_endings_become_lf() {
        assert_eq!(dec(b"a\r\nb\rc\n").0, "a\nb\nc\n");
    }

    #[test]
    fn boms_at_line_starts_are_removed() {
        assert_eq!(dec("# A\r\n\u{FEFF}# B\n x\u{FEFF}y".as_bytes()).0, "# A\n# B\n x\u{FEFF}y");
    }

    #[test]
    fn code_pages_map_to_encodings() {
        assert_eq!(encoding_for_code_page(1251), Some(encoding_rs::WINDOWS_1251));
        assert_eq!(encoding_for_code_page(936), Some(encoding_rs::GBK));
        assert_eq!(encoding_for_code_page(65001), None);
    }
}
