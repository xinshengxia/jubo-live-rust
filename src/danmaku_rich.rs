//! Rich-media helpers shared by the danmaku decoders and the overlay.
//!
//! Three independent mechanisms are modelled here, each ported from the legacy
//! C# implementation but re-implemented against synthetic fixtures only:
//!
//! - platform emoticon dictionaries: literal text codes such as [大笑] map to an
//!   image URL supplied by the platform (danmaku-14-emoticon.cs);
//! - bounded remote image dimensions read from PNG/JPEG/GIF/BMP/WebP headers
//!   before any decoder allocates pixels (danmaku-image-header.cs);
//! - the optional colour emoji sprite-sheet manifest: cell geometry, variant
//!   cells, regional-indicator flags, skin-tone modifiers and ZWJ sequences
//!   (danmaku-7-emoji.cs). No platform or font artwork is bundled: the sheet
//!   remains an externally generated local asset and the text fallback is used
//!   when it is absent.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Longest accepted bracketed emoticon code, in characters (the platform's
/// longest observed code is seven, so this is generous but bounded).
pub const MAX_EMOTICON_CODE_CHARS: usize = 24;
pub const MAX_EMOTICONS: usize = 1024;
pub const MAX_EMOTICON_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 2048;
const MAX_UNKNOWN_CODES: usize = 200;

// ---------------------------------------------------------------------------
// Platform-provided event text
// ---------------------------------------------------------------------------

fn decode_entity(entity: &str) -> Option<&'static str> {
    match entity.to_ascii_lowercase().as_str() {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" | "#39" => Some("'"),
        "nbsp" => Some(" "),
        _ => None,
    }
}

/// Normalises one platform event text: bounded HTML/XML tags are stripped,
/// the common entities are decoded, line breaks become spaces, control
/// characters are dropped and the result is clipped to max_chars.
pub fn sanitize_event_text(raw: &str, max_chars: usize) -> String {
    let mut output = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for _ in 0..64 {
                match chars.next() {
                    Some('>') => {
                        closed = true;
                        break;
                    }
                    Some(next) if !next.is_whitespace() => tag.push(next),
                    Some(_) => tag.push(' '),
                    None => break,
                }
            }
            let looks_like_tag =
                tag.starts_with('/') || tag.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
            if closed && looks_like_tag {
                continue;
            }
            output.push('<');
            output.push_str(&tag);
            if closed {
                output.push('>');
            }
            continue;
        }
        if ch == '&' {
            let mut entity = String::new();
            let mut closed = false;
            for _ in 0..8 {
                match chars.next() {
                    Some(';') => {
                        closed = true;
                        break;
                    }
                    Some(next) => entity.push(next),
                    None => break,
                }
            }
            if closed {
                if let Some(decoded) = decode_entity(&entity) {
                    output.push_str(decoded);
                    continue;
                }
            }
            output.push('&');
            output.push_str(&entity);
            if closed {
                output.push(';');
            }
            continue;
        }
        if ch.is_control() {
            if matches!(ch, '\r' | '\n' | '\t') {
                output.push(' ');
            }
            continue;
        }
        output.push(ch);
    }
    output.trim().chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// Bounded remote image dimensions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let part = bytes.get(at..at + 3)?;
    Some(u32::from(part[0]) | (u32::from(part[1]) << 8) | (u32::from(part[2]) << 16))
}
fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn dimensions(width: u32, height: u32) -> Option<ImageDimensions> {
    (width > 0 && height > 0).then_some(ImageDimensions { width, height })
}

fn webp_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    let end = 8usize.checked_add(le32(bytes, 4)? as usize)?;
    if end > bytes.len() || end < 12 {
        return None;
    }
    let mut cursor = 12;
    while cursor + 8 <= end {
        let fourcc = bytes.get(cursor..cursor + 4)?;
        let length = le32(bytes, cursor + 4)? as usize;
        let body = cursor + 8;
        if length > end - body {
            return None;
        }
        if fourcc == b"VP8X" {
            if length >= 10 {
                let width = 1 + le24(bytes, body + 4)?;
                let height = 1 + le24(bytes, body + 7)?;
                return dimensions(width, height);
            }
            return None;
        }
        if fourcc == b"VP8L" {
            if length < 5 || bytes.get(body) != Some(&0x2f) {
                return None;
            }
            let bits = le32(bytes, body + 1)?;
            return dimensions(1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff));
        }
        if fourcc == b"VP8 " {
            // Lossy frame header: 3-byte frame tag, start code 9d 01 2a, then
            // 14-bit width and height. Only the still-image layout is needed.
            if length < 10 || bytes.get(body + 3..body + 6) != Some(&[0x9d, 0x01, 0x2a]) {
                return None;
            }
            let width = u32::from(le16(bytes, body + 6)? & 0x3fff);
            let height = u32::from(le16(bytes, body + 8)? & 0x3fff);
            return dimensions(width, height);
        }
        // Chunk bodies are padded to an even size.
        let padded = length + (length & 1);
        cursor += 8 + padded;
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.get(0..2) != Some(&[0xff, 0xd8]) {
        return None;
    }
    let mut cursor = 2;
    while cursor < bytes.len() {
        if bytes.get(cursor)? != &0xff {
            return None;
        }
        cursor += 1;
        while bytes.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let marker = *bytes.get(cursor)?;
        cursor += 1;
        if marker == 0xd9 || marker == 0xda {
            // End of image or start of scan: no dimensions remain to read.
            return None;
        }
        if marker == 1 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let length = usize::from(be16(bytes, cursor)?);
        if length < 2 || cursor + length > bytes.len() {
            return None;
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if length < 8 {
                return None;
            }
            let height = u32::from(be16(bytes, cursor + 3)?);
            let width = u32::from(be16(bytes, cursor + 5)?);
            return dimensions(width, height);
        }
        cursor += length;
    }
    None
}

/// Reads the container dimensions of the image formats accepted for remote
/// danmaku/SC artwork. Unknown or malformed input yields None.
pub fn image_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 {
        return None;
    }
    if bytes.get(0..8) == Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        if bytes.len() >= 24 && bytes.get(12..16) == Some(b"IHDR") {
            return dimensions(be32(bytes, 16)?, be32(bytes, 20)?);
        }
        return None;
    }
    if bytes.get(0..6) == Some(b"GIF87a") || bytes.get(0..6) == Some(b"GIF89a") {
        return dimensions(u32::from(le16(bytes, 6)?), u32::from(le16(bytes, 8)?));
    }
    if bytes.get(0..2) == Some(b"BM") && bytes.len() >= 26 {
        let dib = le32(bytes, 14)?;
        if dib == 12 {
            return dimensions(u32::from(le16(bytes, 18)?), u32::from(le16(bytes, 20)?));
        }
        if dib >= 40 {
            let width = i32::try_from(le32(bytes, 18)?).ok()?;
            let height = i32::try_from(le32(bytes, 22)?).ok()?;
            return dimensions(u32::try_from(width).ok()?, height.unsigned_abs());
        }
        return None;
    }
    if bytes.get(0..4) == Some(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return webp_dimensions(bytes);
    }
    jpeg_dimensions(bytes)
}

/// Whether a remote image is acceptable for decoding: bounded wire size and a
/// positive container size inside the decoder limits.
pub fn image_within_limits(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_IMAGE_BYTES
        && image_dimensions(bytes).is_some_and(|size| {
            size.width <= MAX_IMAGE_DIMENSION && size.height <= MAX_IMAGE_DIMENSION
        })
}

// ---------------------------------------------------------------------------
// Platform emoticon dictionary (text code -> image URL)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoticonDictionary {
    by_code: BTreeMap<String, String>,
    /// Count-only diagnostics stay local; they are never sent between threads.
    #[serde(skip)]
    unknown: BTreeSet<String>,
}

impl EmoticonDictionary {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.by_code.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }
    pub fn clear(&mut self) {
        self.by_code.clear();
        self.unknown.clear();
    }
    /// Codes the platform did not provide. Bounded so a hostile room cannot
    /// grow memory, and useful to answer "why is this bracketed code not an
    /// image".
    pub fn unknown_codes(&self) -> &BTreeSet<String> {
        &self.unknown
    }

    /// Adds one code. The brackets are optional and normalised; a code or URL
    /// that is empty, oversized or contains control characters is rejected.
    pub fn add(&mut self, code: &str, url: &str) -> bool {
        if self.by_code.len() >= MAX_EMOTICONS {
            return false;
        }
        let code = code.trim();
        if code.is_empty() || url.is_empty() || url.len() > 2048 {
            return false;
        }
        if url.chars().any(char::is_control) {
            return false;
        }
        let mut normalized = if code.starts_with('[') {
            code.to_owned()
        } else {
            format!("[{code}")
        };
        if !normalized.ends_with(']') {
            normalized.push(']');
        }
        if normalized.chars().count() > MAX_EMOTICON_CODE_CHARS + 2 {
            return false;
        }
        self.by_code.insert(normalized, url.to_owned());
        true
    }

    pub fn get(&self, code: &str) -> Option<&str> {
        self.by_code.get(code).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_code.iter().map(|(code, url)| (code.as_str(), url.as_str()))
    }

    fn note_unknown(&mut self, code: &str) {
        if self.by_code.is_empty() || self.unknown.len() >= MAX_UNKNOWN_CODES {
            return;
        }
        self.unknown.insert(code.to_owned());
    }

    /// Matches a bracketed code starting at the byte index pointed at by start,
    /// which must be an opening bracket. Returns the URL and the byte index
    /// just after the closing bracket.
    pub fn try_match_at(&self, text: &str, start: usize) -> Option<(String, usize)> {
        let code = self.code_at(text, start)?;
        let url = self.by_code.get(&code)?;
        Some((url.clone(), start + code.len()))
    }

    fn code_at(&self, text: &str, start: usize) -> Option<String> {
        if text.as_bytes().get(start) != Some(&b'[') {
            return None;
        }
        let rest = text.get(start + 1..)?;
        let mut length = 0;
        for (index, ch) in rest.char_indices() {
            if ch == ']' {
                return text.get(start..start + 1 + index + 1).map(str::to_owned);
            }
            length += 1;
            if length > MAX_EMOTICON_CODE_CHARS {
                return None;
            }
        }
        None
    }

    fn match_chars(&mut self, chars: &[char], start: usize) -> Option<(String, usize)> {
        if chars.get(start) != Some(&'[') {
            return None;
        }
        let mut code = String::from('[');
        for index in start + 1..chars.len() {
            let ch = chars[index];
            if ch == ']' {
                code.push(']');
                return match self.by_code.get(&code) {
                    Some(url) => Some((url.clone(), index + 1)),
                    None => {
                        self.note_unknown(&code);
                        None
                    }
                };
            }
            if code.chars().count() > MAX_EMOTICON_CODE_CHARS + 1 {
                return None;
            }
            code.push(ch);
        }
        None
    }
}

fn approved_emoticon_url(url: &str) -> bool {
    crate::images::approved_url(url).is_ok()
}

/// Parses the B站 room emoticon response
/// {"code":0,"data":{"packages":[{"emoticons":[{"emoji":"[大笑]","url":"..."}]}]}}.
/// Only already-approved CDN URLs are kept.
pub fn parse_bili_emoticons(json: &str) -> Result<EmoticonDictionary> {
    ensure!(
        json.len() <= MAX_EMOTICON_JSON_BYTES,
        "表情字典响应超过大小上限"
    );
    let value: serde_json::Value = serde_json::from_str(json)?;
    if let Some(code) = value["code"].as_i64() {
        ensure!(code == 0, "B站表情接口返回错误码 {code}");
    }
    let mut dictionary = EmoticonDictionary::new();
    // The live API nests the package list at data.data (legacy C# parsed the
    // same path); older fixtures used data.packages, so accept both.
    let packages = value["data"]["data"]
        .as_array()
        .or_else(|| value["data"]["packages"].as_array());
    if let Some(packages) = packages {
        for package in packages.iter().take(64) {
            let Some(items) = package["emoticons"].as_array() else {
                continue;
            };
            for item in items.iter().take(MAX_EMOTICONS) {
                let code = item["emoji"].as_str().unwrap_or("");
                let url = item["url"].as_str().unwrap_or("");
                if approved_emoticon_url(url) {
                    dictionary.add(code, url);
                }
            }
        }
    }
    Ok(dictionary)
}

/// Parses the Douyin emoji/list response
/// {"emoji_list":[{"display_name":"[微笑]","emoji_url":{"url_list":["..."]}}]}.
/// Only already-approved CDN URLs are kept; the live response lists several
/// CDN mirrors per emoji, so every entry is tried until one is approved.
pub fn parse_douyin_emoticons(json: &str) -> Result<EmoticonDictionary> {
    ensure!(
        json.len() <= MAX_EMOTICON_JSON_BYTES,
        "表情字典响应超过大小上限"
    );
    let value: serde_json::Value = serde_json::from_str(json)?;
    let mut dictionary = EmoticonDictionary::new();
    if let Some(items) = value["emoji_list"].as_array() {
        for item in items.iter().take(MAX_EMOTICONS) {
            let code = item["display_name"].as_str().unwrap_or("").trim();
            if code.is_empty() {
                continue;
            }
            let Some(urls) = item["emoji_url"]["url_list"].as_array() else {
                continue;
            };
            if let Some(url) = urls
                .iter()
                .filter_map(|url| url.as_str())
                .find(|url| approved_emoticon_url(url))
            {
                dictionary.add(code, url);
            }
        }
    }
    Ok(dictionary)
}

// ---------------------------------------------------------------------------
// Colour emoji sprite-sheet manifest
// ---------------------------------------------------------------------------

/// Geometry and codepoint tables of the optional local colour emoji sheet.
/// The defaults mirror the legacy generator's defaults so a missing or partial
/// manifest still produces the documented layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiManifest {
    pub cell: u32,
    pub columns: u32,
    pub plain_count: u32,
    pub flag_start: u32,
    pub flag_count: u32,
    pub flag_base: u32,
    pub sequence_start: u32,
    pub sequence_count: u32,
    pub plain: Vec<(u32, u32)>,
    pub variant: Vec<(u32, u32)>,
    sequences: BTreeMap<String, u32>,
}

pub const DEFAULT_PLAIN_RANGES: [(u32, u32); 20] = [
    (0x2600, 0x27BF),
    (0x2B00, 0x2BFF),
    (0x2300, 0x23FF),
    (0x1F000, 0x1F0FF),
    (0x1F100, 0x1F2FF),
    (0x1F300, 0x1F5FF),
    (0x1F600, 0x1F64F),
    (0x1F680, 0x1F6FF),
    (0x1F900, 0x1F9FF),
    (0x1FA70, 0x1FAFF),
    (0x00A9, 0x00A9),
    (0x00AE, 0x00AE),
    (0x203C, 0x203C),
    (0x2049, 0x2049),
    (0x2122, 0x2122),
    (0x2139, 0x2139),
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
];

pub const DEFAULT_VARIANT_RANGES: [(u32, u32); 12] = [
    (0x2600, 0x27BF),
    (0x2B00, 0x2BFF),
    (0x00A9, 0x00A9),
    (0x00AE, 0x00AE),
    (0x203C, 0x203C),
    (0x2049, 0x2049),
    (0x2122, 0x2122),
    (0x2139, 0x2139),
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
];

impl Default for EmojiManifest {
    fn default() -> Self {
        Self {
            cell: 40,
            columns: 48,
            plain_count: 3114,
            flag_start: 3828,
            flag_count: 0,
            flag_base: 0x1F1E6,
            sequence_start: 3828,
            sequence_count: 0,
            plain: DEFAULT_PLAIN_RANGES.to_vec(),
            variant: DEFAULT_VARIANT_RANGES.to_vec(),
            sequences: BTreeMap::new(),
        }
    }
}

fn parse_ranges(text: &str) -> Option<Vec<(u32, u32)>> {
    let mut ranges = Vec::new();
    for piece in text.split(',') {
        let item = piece.trim();
        if item.is_empty() {
            continue;
        }
        let (start, end) = match item.split_once('-') {
            Some((start, end)) => (
                u32::from_str_radix(start.trim(), 16).ok()?,
                u32::from_str_radix(end.trim(), 16).ok()?,
            ),
            None => {
                let value = u32::from_str_radix(item, 16).ok()?;
                (value, value)
            }
        };
        if start > end {
            return None;
        }
        ranges.push((start, end));
    }
    (!ranges.is_empty()).then_some(ranges)
}

fn parse_sequences(text: &str) -> BTreeMap<String, u32> {
    let mut sequences = BTreeMap::new();
    for (offset, entry) in text.split(',').enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut key = String::new();
        let mut valid = true;
        for part in entry.split('-') {
            let Ok(code) = u32::from_str_radix(part.trim(), 16) else {
                valid = false;
                break;
            };
            // Joiner and variation selector never take part in matching.
            if matches!(code, 0x200D | 0xFE0F) {
                continue;
            }
            match char::from_u32(code) {
                Some(ch) => key.push(ch),
                None => {
                    valid = false;
                    break;
                }
            }
        }
        if valid && !key.is_empty() {
            sequences.insert(key, offset as u32);
        }
    }
    sequences
}

impl EmojiManifest {
    /// Parses the generated manifest. Unknown or malformed lines are ignored;
    /// a malformed range/sequence list keeps the built-in default instead of
    /// leaving a half-applied table.
    pub fn parse(text: &str) -> Result<Self> {
        let mut manifest = Self::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            let number = || value.parse::<u32>().ok();
            match key {
                "CELL" => {
                    if let Some(parsed) = number().filter(|n| *n > 0 && *n <= 512) {
                        manifest.cell = parsed;
                    }
                }
                "COLUMNS" => {
                    if let Some(parsed) = number().filter(|n| *n > 0 && *n <= 512) {
                        manifest.columns = parsed;
                    }
                }
                "PLAINCOUNT" => {
                    if let Some(parsed) = number() {
                        manifest.plain_count = parsed;
                    }
                }
                "FLAGSTART" => {
                    if let Some(parsed) = number() {
                        manifest.flag_start = parsed;
                    }
                }
                "FLAGCOUNT" => {
                    if let Some(parsed) = number() {
                        manifest.flag_count = parsed;
                    }
                }
                "FLAGBASE" => {
                    if let Ok(parsed) = u32::from_str_radix(value, 16) {
                        manifest.flag_base = parsed;
                    }
                }
                "SEQSTART" => {
                    if let Some(parsed) = number() {
                        manifest.sequence_start = parsed;
                    }
                }
                "SEQCOUNT" => {
                    if let Some(parsed) = number() {
                        manifest.sequence_count = parsed;
                    }
                }
                "PLAIN" => {
                    if let Some(parsed) = parse_ranges(value) {
                        manifest.plain = parsed;
                    }
                }
                "VS" => {
                    if let Some(parsed) = parse_ranges(value) {
                        manifest.variant = parsed;
                    }
                }
                "SEQ" => {
                    manifest.sequences = parse_sequences(value);
                }
                _ => {}
            }
        }
        Ok(manifest)
    }

    fn index_of(ranges: &[(u32, u32)], code_point: u32) -> Option<u32> {
        let mut offset = 0;
        for (start, end) in ranges {
            if code_point >= *start && code_point <= *end {
                return Some(offset + (code_point - start));
            }
            offset += end - start + 1;
        }
        None
    }

    pub fn has_variant(&self, code_point: u32) -> bool {
        Self::index_of(&self.variant, code_point).is_some()
    }
    pub fn is_regional_indicator(&self, code_point: u32) -> bool {
        code_point >= self.flag_base && code_point <= self.flag_base + 25
    }
    pub fn is_skin_tone(code_point: u32) -> bool {
        (0x1F3FB..=0x1F3FF).contains(&code_point)
    }

    /// Cell pixel offsets for one codepoint. variant must only be true when
    /// the source text actually carried U+FE0F.
    pub fn cell_for(&self, code_point: u32, variant: bool) -> Option<(u32, u32)> {
        let index = if variant {
            Self::index_of(&self.variant, code_point).map(|offset| offset + self.plain_count)
        } else {
            None
        };
        let index = match index {
            Some(index) => index,
            None => Self::index_of(&self.plain, code_point)?,
        };
        self.cell_at(index)
    }

    pub fn flag_cell(&self, first: u32, second: u32) -> Option<(u32, u32)> {
        if self.flag_count == 0 {
            return None;
        }
        if !self.is_regional_indicator(first) || !self.is_regional_indicator(second) {
            return None;
        }
        let index = self.flag_start + (first - self.flag_base) * 26 + (second - self.flag_base);
        self.cell_at(index)
    }

    pub fn sequence_cell(&self, offset: u32) -> Option<(u32, u32)> {
        if offset >= self.sequence_count {
            return None;
        }
        self.cell_at(self.sequence_start + offset)
    }

    pub fn cell_at(&self, index: u32) -> Option<(u32, u32)> {
        if self.columns == 0 {
            return None;
        }
        let column = index % self.columns;
        let row = index / self.columns;
        Some((column * self.cell, row * self.cell))
    }

    /// Tries to match a ZWJ sequence starting at chars[start]. Returns the
    /// sequence offset and how many characters were consumed (including any
    /// trailing U+FE0F).
    pub fn match_sequence(&self, chars: &[char], start: usize) -> Option<(u32, usize)> {
        if self.sequences.is_empty() {
            return None;
        }
        let mut code_points = Vec::new();
        let mut ends = Vec::new();
        let mut index = start;
        let mut after_joiner = false;
        while index < chars.len() {
            let code = chars[index] as u32;
            if code == 0x200D {
                index += 1;
                after_joiner = true;
                continue;
            }
            if code == 0xFE0F || Self::is_skin_tone(code) {
                index += 1;
                continue;
            }
            if code == 0x20E3 {
                code_points.push(code);
                index += 1;
                ends.push(index);
                after_joiner = false;
                continue;
            }
            if !code_points.is_empty() && !after_joiner {
                break;
            }
            code_points.push(code);
            index += 1;
            ends.push(index);
            after_joiner = false;
        }
        for take in (2..=code_points.len()).rev() {
            let key: String = code_points[..take]
                .iter()
                .filter_map(|code| char::from_u32(*code))
                .collect();
            let Some(offset) = self.sequences.get(&key) else {
                continue;
            };
            let mut end = ends[take - 1];
            while end < chars.len() && chars[end] == '\u{FE0F}' {
                end += 1;
            }
            return Some((*offset, end));
        }
        None
    }

    /// Matches one emoji cluster at chars[start].
    ///
    /// Ok(Some(..)) means a cell; Ok(None) means "skip this character" (skin
    /// tone modifier); Err(()) means "not an emoji, render the text".
    pub fn match_at(
        &self,
        chars: &[char],
        start: usize,
    ) -> std::result::Result<Option<((u32, u32), usize)>, ()> {
        let Some(&first) = chars.get(start) else {
            return Err(());
        };
        let code_point = first as u32;
        if Self::is_skin_tone(code_point) {
            return Ok(None);
        }
        if let Some((offset, end)) = self.match_sequence(chars, start) {
            return Ok(self.sequence_cell(offset).map(|cell| (cell, end)));
        }
        if self.is_regional_indicator(code_point) {
            if let Some(&second) = chars.get(start + 1) {
                if let Some(cell) = self.flag_cell(code_point, second as u32) {
                    return Ok(Some((cell, start + 2)));
                }
            }
        }
        if chars.get(start + 1) == Some(&'\u{FE0F}') && self.has_variant(code_point) {
            return Ok(self
                .cell_for(code_point, true)
                .map(|cell| (cell, start + 2)));
        }
        Ok(self
            .cell_for(code_point, false)
            .map(|cell| (cell, start + 1)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSegment {
    /// Plain text rendered with the overlay font.
    Text(String),
    /// Remote image emoticon; the original code is kept for the text fallback
    /// when the image has not arrived yet.
    Emoticon { code: String, url: String },
    /// Sprite-sheet cell (pixel offsets inside the local sheet).
    EmojiCell { x: u32, y: u32 },
}

/// Splits one danmaku text into text runs, platform image emoticons and local
/// colour-emoji cells. With neither a dictionary nor a manifest the result is a
/// single text run, which is also the documented fallback.
pub fn split_text_segments(
    text: &str,
    dictionary: Option<&EmoticonDictionary>,
    manifest: Option<&EmojiManifest>,
) -> Vec<TextSegment> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments = Vec::new();
    let mut buffer = String::new();
    let mut index = 0;
    let mut dictionary = dictionary.cloned();
    while index < chars.len() {
        let ch = chars[index];
        if ch == '[' {
            if let Some(dict) = dictionary.as_mut() {
                if let Some((url, end)) = dict.match_chars(&chars, index) {
                    if !buffer.is_empty() {
                        segments.push(TextSegment::Text(std::mem::take(&mut buffer)));
                    }
                    let code: String = chars[index..end].iter().collect();
                    segments.push(TextSegment::Emoticon { code, url });
                    index = end;
                    continue;
                }
            }
        }
        if let Some(manifest) = manifest {
            match manifest.match_at(&chars, index) {
                Ok(Some(((x, y), end))) => {
                    if !buffer.is_empty() {
                        segments.push(TextSegment::Text(std::mem::take(&mut buffer)));
                    }
                    segments.push(TextSegment::EmojiCell { x, y });
                    index = end;
                    continue;
                }
                Ok(None) => {
                    if EmojiManifest::is_skin_tone(ch as u32) {
                        // Skin-tone modifier: consumed without emitting a cell.
                        index += 1;
                        continue;
                    }
                    // Not an emoji after all: fall through and keep the text.
                }
                Err(()) => {}
            }
        }
        buffer.push(ch);
        index += 1;
    }
    if !buffer.is_empty() {
        segments.push(TextSegment::Text(buffer));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_text_strips_tags_decodes_entities_and_bounds_length() {
        assert_eq!(
            sanitize_event_text("<b>合成</b> &amp; 公告&lt;1&gt;", 40),
            "合成 & 公告<1>"
        );
        // Each line break becomes one space, matching the legacy replacement.
        assert_eq!(sanitize_event_text("a\r\nb\tc\u{7}", 10), "a  b c");
        assert_eq!(sanitize_event_text("x<y", 10), "x<y");
        assert_eq!(sanitize_event_text("&unknown; &amp", 40), "&unknown; &amp");
        assert_eq!(sanitize_event_text("abcdef", 4), "abcd");
        assert_eq!(sanitize_event_text("   ", 10), "");
    }

    #[test]
    fn image_headers_report_positive_container_dimensions_only() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13_u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&3_u32.to_be_bytes());
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(
            image_dimensions(&png),
            Some(ImageDimensions {
                width: 3,
                height: 2
            })
        );
        assert!(image_within_limits(&png));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&7_u16.to_le_bytes());
        gif.extend_from_slice(&9_u16.to_le_bytes());
        gif.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            image_dimensions(&gif),
            Some(ImageDimensions {
                width: 7,
                height: 9
            })
        );

        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08];
        jpeg.extend_from_slice(&9_u16.to_be_bytes());
        jpeg.extend_from_slice(&11_u16.to_be_bytes());
        jpeg.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0]);
        assert_eq!(
            image_dimensions(&jpeg),
            Some(ImageDimensions {
                width: 11,
                height: 9
            })
        );

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&17_u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8L");
        webp.extend_from_slice(&5_u32.to_le_bytes());
        webp.push(0x2f);
        let bits = 2_u32 | (1_u32 << 14); // width 3, height 2
        webp.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(
            image_dimensions(&webp),
            Some(ImageDimensions {
                width: 3,
                height: 2
            })
        );

        assert!(image_dimensions(b"<html>not an image</html>").is_none());
        assert!(image_dimensions(b"GIF89a\x00").is_none());
        assert!(!image_within_limits(b"RIFF\x00"));
    }

    #[test]
    fn emoticon_codes_are_normalized_and_bounded() {
        let mut dictionary = EmoticonDictionary::new();
        assert!(dictionary.add("cat", "https://i0.hdslb.com/cat.png"));
        assert!(dictionary.add("[dog]", "https://i0.hdslb.com/dog.png"));
        assert_eq!(
            dictionary.get("[cat]"),
            Some("https://i0.hdslb.com/cat.png")
        );
        assert_eq!(dictionary.len(), 2);
        assert!(!dictionary.add("", "https://i0.hdslb.com/x.png"));
        assert!(!dictionary.add("x", ""));
        assert!(!dictionary.add("x", "https://i0.hdslb.com/bad\n.png"));
        assert!(!dictionary.add(&"x".repeat(64), "https://i0.hdslb.com/x.png"));
        assert_eq!(
            dictionary.try_match_at("hi[cat]there", 2).unwrap(),
            ("https://i0.hdslb.com/cat.png".to_string(), 7)
        );
        assert!(dictionary.try_match_at("hi cat there", 2).is_none());
        dictionary.clear();
        assert!(dictionary.is_empty());
    }

    #[test]
    fn platform_emoticon_payloads_keep_only_approved_cdn_urls() {
        let bili = r#"{"code":0,"data":{"packages":[{"emoticons":[
            {"emoji":"[合成笑]","url":"https://i0.hdslb.com/bfs/live/a.png"},
            {"emoji":"[私有]","url":"https://127.0.0.1/a.png"},
            {"emoji":"[外部]","url":"https://evil.invalid/a.png"}]}]}}"#;
        let dictionary = parse_bili_emoticons(bili).unwrap();
        assert_eq!(dictionary.len(), 1);
        assert_eq!(
            dictionary.get("[合成笑]"),
            Some("https://i0.hdslb.com/bfs/live/a.png")
        );
        // Live response shape: packages under data.data (legacy C# path).
        let live_shape = r#"{"code":0,"data":{"data":[{"pkg_name":"合成","emoticons":[
            {"emoji":"[合成泪]","url":"https://i0.hdslb.com/bfs/live/b.png"}]}]}}"#;
        let dictionary = parse_bili_emoticons(live_shape).unwrap();
        assert_eq!(dictionary.len(), 1);
        assert!(dictionary.get("[合成泪]").is_some());
        assert!(parse_bili_emoticons(r#"{"code":-500}"#).is_err());
        assert!(parse_bili_emoticons("<html>").is_err());
        let oversized = format!(
            r#"{{"code":0,"data":{{"packages":[{{"emoticons":[{{"emoji":"[a]","url":"{}"}}]}}]}}}}"#,
            "x".repeat(MAX_EMOTICON_JSON_BYTES + 1)
        );
        assert!(parse_bili_emoticons(&oversized).is_err());

        let douyin = r#"{"emoji_list":[
            {"display_name":"[微笑]","emoji_url":{"url_list":["https://p3.douyinpic.com/a.png"]}},
            {"display_name":"[坏]","emoji_url":{"url_list":["https://127.0.0.1/a.png"]}}]}"#;
        let dictionary = parse_douyin_emoticons(douyin).unwrap();
        assert_eq!(dictionary.len(), 1);
        assert!(dictionary.get("[微笑]").is_some());
        assert!(dictionary.get("[坏]").is_none());
    }

    #[test]
    fn douyin_emoticons_try_every_listed_url_and_skip_empty_codes() {
        let douyin = r#"{"emoji_list":[
            {"display_name":"[捂脸]","emoji_url":{"url_list":[
                "https://evil.invalid/a.png",
                "https://p3.douyinpic.com/approved.png",
                "https://p6.douyinpic.com/second.png"]}},
            {"display_name":"[坏]","emoji_url":{"url_list":[
                "https://evil.invalid/b.png",
                "https://127.0.0.1/b.png"]}},
            {"display_name":"   ","emoji_url":{"url_list":[
                "https://p3.douyinpic.com/blank.png"]}}]}"#;
        let dictionary = parse_douyin_emoticons(douyin).unwrap();
        assert_eq!(dictionary.len(), 1);
        assert_eq!(
            dictionary.get("[捂脸]"),
            Some("https://p3.douyinpic.com/approved.png")
        );
        // All mirrors unapproved: the code stays literal text.
        assert!(dictionary.get("[坏]").is_none());
    }

    fn sample_manifest() -> EmojiManifest {
        EmojiManifest::parse(
            "# synthetic\nCELL=2\nCOLUMNS=2\nPLAINCOUNT=2\nPLAIN=2764-2765\nVS=2764-2764\n\
             FLAGCOUNT=4\nFLAGSTART=4\nFLAGBASE=1F1E6\nSEQSTART=8\nSEQCOUNT=1\n\
             SEQ=1F468-200D-1F469\n",
        )
        .unwrap()
    }

    #[test]
    fn sprite_manifest_maps_plain_variant_flag_and_sequence_cells() {
        let manifest = sample_manifest();
        assert_eq!((manifest.cell, manifest.columns), (2, 2));
        assert_eq!(manifest.plain, vec![(0x2764, 0x2765)]);
        assert_eq!(manifest.cell_for(0x2764, false), Some((0, 0)));
        assert_eq!(manifest.cell_for(0x2765, false), Some((2, 0)));
        assert!(manifest.has_variant(0x2764));
        assert!(!manifest.has_variant(0x2765));
        // Variant cells start after every plain cell (index 2 -> row 1).
        assert_eq!(manifest.cell_for(0x2764, true), Some((0, 2)));
        assert!(manifest.flag_cell(0x1F1E6, 0x1F1E6).is_some());
        assert!(manifest.flag_cell(0x1F1E6, 0x1F200).is_none());
        assert_eq!(manifest.sequence_cell(0), manifest.cell_at(8));
        assert!(manifest.sequence_cell(1).is_none());

        let chars: Vec<char> = "👨‍👩".chars().collect();
        assert_eq!(manifest.match_sequence(&chars, 0), Some((0, 3)));
        let with_selector: Vec<char> = "❤️".chars().collect();
        let matched = manifest.match_at(&with_selector, 0).unwrap().unwrap();
        assert_eq!(matched.0, (0, 2));
        assert_eq!(matched.1, 2);
        // A bare heart stays a plain cell.
        let plain: Vec<char> = "❤".chars().collect();
        assert_eq!(manifest.match_at(&plain, 0).unwrap().unwrap().0, (0, 0));
    }

    #[test]
    fn text_segmentation_preserves_order_and_falls_back_to_text() {
        let mut dictionary = EmoticonDictionary::new();
        dictionary.add("cat", "https://i0.hdslb.com/cat.png");
        assert_eq!(
            split_text_segments("hi[cat]there", Some(&dictionary), None),
            vec![
                TextSegment::Text("hi".into()),
                TextSegment::Emoticon {
                    code: "[cat]".into(),
                    url: "https://i0.hdslb.com/cat.png".into()
                },
                TextSegment::Text("there".into())
            ]
        );
        assert_eq!(
            split_text_segments("[unknown] hi", Some(&dictionary), None),
            vec![TextSegment::Text("[unknown] hi".into())]
        );
        assert_eq!(
            split_text_segments("hi", Some(&dictionary), None),
            vec![TextSegment::Text("hi".into())]
        );
        let manifest = sample_manifest();
        assert_eq!(
            split_text_segments("a❤️b👨‍👩c", None, Some(&manifest)),
            vec![
                TextSegment::Text("a".into()),
                TextSegment::EmojiCell { x: 0, y: 2 },
                TextSegment::Text("b".into()),
                TextSegment::EmojiCell { x: 0, y: 8 },
                TextSegment::Text("c".into())
            ]
        );
        assert_eq!(split_text_segments("", None, None), Vec::new());
    }

    #[test]
    fn unknown_emoticon_codes_are_recorded_once_and_bounded() {
        let mut dictionary = EmoticonDictionary::new();
        dictionary.add("known", "https://i0.hdslb.com/known.png");
        let chars: Vec<char> = "[missing]".chars().collect();
        assert!(dictionary.match_chars(&chars, 0).is_none());
        assert_eq!(dictionary.unknown_codes().len(), 1);
        assert!(dictionary.match_chars(&chars, 0).is_none());
        assert_eq!(dictionary.unknown_codes().len(), 1);
        for index in 0..MAX_UNKNOWN_CODES + 10 {
            let code: Vec<char> = format!("[x{index}]").chars().collect();
            dictionary.match_chars(&code, 0);
        }
        assert_eq!(dictionary.unknown_codes().len(), MAX_UNKNOWN_CODES);
    }
}

// ---------------------------------------------------------------------------
// Sprite-sheet contract tests (task-18 R2)
// ---------------------------------------------------------------------------
//
// These tests pin the *manifest* view of the colour emoji sheet: the txt file
// is the single authority for codepoint -> cell coordinates (legacy C#
// behaviour and the generator's own ordering). A separate investigation note
// (task notes/emoji-plain-index-investigation.md) records that the shipped
// PNG appears shifted by six cells for the 1F300-1F5FF block; until the asset
// pipeline is re-run with a pinned browser/font, keep this contract on the txt
// so a future sheet regeneration is validated automatically.
#[cfg(test)]
mod emoji_sheet_contract {
    use super::EmojiManifest;

    fn asset_bytes(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(name);
        std::fs::read(path).expect("bundled asset")
    }

    fn manifest() -> EmojiManifest {
        let text = String::from_utf8(asset_bytes("emoji-sheet.txt")).expect("utf8 manifest");
        EmojiManifest::parse(&text).expect("manifest parses")
    }

    /// codepoint -> expected (x, y) cell for the shipped manifest.
    const SAMPLES: [(u32, (u32, u32)); 10] = [
        (0x1F389, (1640, 1520)),
        (0x1F38A, (1680, 1520)),
        (0x1F602, (80, 2080)),
        (0x1F600, (0, 2080)),
        (0x1F44D, (1800, 1680)),
        (0x1F40D, (1160, 1640)),
        (0x1F3AE, (1200, 1560)),
        (0x1F525, (840, 1880)),
        (0x2764, (800, 280)),
        (0x1F3B2, (1360, 1560)),
    ];

    #[test]
    fn manifest_matches_asset_shape_and_capacity() {
        let manifest = manifest();
        assert_eq!(manifest.cell, 40);
        assert_eq!(manifest.columns, 48);
        assert_eq!(manifest.plain_count, 3114);
        let expected_total = manifest.plain_count + 714 + manifest.flag_count + manifest.sequence_count;
        let needed_rows = expected_total.div_ceil(manifest.columns);
        let width = manifest.columns * manifest.cell;
        let height = needed_rows * manifest.cell;
        assert_eq!(width, 1920);
        assert_eq!(height, 3360, "sheet rows must match manifest capacity");
    }

    #[test]
    fn codepoint_to_cell_contract_for_representative_emoji() {
        let manifest = manifest();
        for (codepoint, expected) in SAMPLES {
            let cell = manifest
                .cell_for(codepoint, false)
                .unwrap_or_else(|| panic!("codepoint {codepoint:X} must map to a cell"));
            assert_eq!(cell, expected, "codepoint {codepoint:X}");
        }
        // Unknown / unassigned codepoints stay unsupported instead of guessing.
        assert!(manifest.cell_for(0x10FFFF, false).is_none());
    }

    #[test]
    fn variant_cells_use_the_variant_block() {
        let manifest = manifest();
        let plain = manifest.cell_for(0x2764, false).expect("plain heart");
        let variant = manifest.cell_for(0x2764, true).expect("variant heart");
        assert_ne!(plain, variant);
        assert!(variant.1 > plain.1, "variant block starts after the plain block");
        // A codepoint outside the VS ranges falls back to its plain cell.
        assert_eq!(manifest.cell_for(0x1F389, true), manifest.cell_for(0x1F389, false));
    }

    #[test]
    fn shipped_png_covers_every_manifest_cell() {
        let manifest = manifest();
        let bytes = asset_bytes("emoji-sheet.png");
        let image = image::load_from_memory(&bytes).expect("png decodes");
        assert_eq!(image.width(), manifest.columns * manifest.cell);
        let total = manifest.plain_count
            + 714
            + manifest.flag_count
            + manifest.sequence_count;
        let rows = total.div_ceil(manifest.columns);
        assert_eq!(image.height(), rows * manifest.cell);

        // Every sampled plain cell must have visible ink (a fully transparent
        // cell would mean the sheet and manifest disagree about capacity).
        let rgba = image.to_rgba8();
        for (codepoint, (x, y)) in SAMPLES {
            let mut ink = 0u32;
            for row in y..(y + manifest.cell) {
                for column in x..(x + manifest.cell) {
                    if rgba.get_pixel(column, row).0[3] > 16 {
                        ink += 1;
                    }
                }
            }
            assert!(
                ink > 16,
                "codepoint {codepoint:X} cell at ({x},{y}) is empty (manifest/sheet mismatch)"
            );
        }
    }
}
