//! Port of Go `api/encoder/stringtable.go`.
//!
//! PORT: Go `int` byte lengths and offsets in this file are `usize` (they
//! index byte buffers). Go string slices and comparisons work on bytes, so
//! the port compares bytes and never slices a `&str`.
//!
//! PORT: the table holds Go bytes, not the port form (see
//! `GO_STRING_MARKER`), so the client reads the same bytes and offsets as
//! from Go: a lone surrogate is its 3 WTF-8 bytes and an invalid source byte
//! is 1 byte. Node `pos` and `end` are port offsets in the file text, so
//! `add` changes them to Go offsets first.

use std::borrow::Cow;

use crate::api::encoder::prelude::*;

// Go: api/encoder/stringtable.go:9 stringTable
pub struct StringTable {
    // PORT: the file text. Its Go bytes are `go_file_bytes` when it has a
    // unit, else its own bytes (`file_bytes`).
    pub file_text: FileText,
    go_file_bytes: Option<Vec<u8>>,
    // PORT: Go `*strings.Builder`, with Go bytes.
    pub other_strings: Vec<u8>,
    // offsets are pos/end pairs
    pub offsets: Vec<u32>,
    // PORT: for each unit of the file text (see `GO_STRING_MARKER`), the
    // port offset after it and the number of port bytes before that offset
    // that Go does not have. Empty for most files.
    units: Vec<(usize, usize)>,
}

// Go: api/encoder/stringtable.go:16 newStringTable
pub fn new_string_table(file_text: impl Into<FileText>, string_count: usize) -> StringTable {
    let file_text: FileText = file_text.into();
    let builder = Vec::new();
    let go_file_bytes = match go_string_bytes(&file_text) {
        Cow::Borrowed(_) => None,
        Cow::Owned(bytes) => Some(bytes),
    };
    // PERF: (apiperf2) a text with no marker has no unit: one scan of
    // the file text, not two.
    let units = if go_file_bytes.is_some() {
        port_units(&file_text)
    } else {
        Vec::new()
    };
    StringTable {
        units,
        file_text,
        go_file_bytes,
        other_strings: builder,
        offsets: Vec::with_capacity(string_count * 2),
    }
}

/// PORT: the `units` table of `StringTable` for the port form `text`.
fn port_units(text: &str) -> Vec<(usize, usize)> {
    let mut units = Vec::new();
    let mut extra = 0usize;
    let mut after = 0usize;
    for (at, _) in text.match_indices(GO_STRING_MARKER) {
        // The second M of an M + M unit.
        if at < after {
            continue;
        }
        let (unit, size) = go_unit_at(text, at);
        after = at + size;
        extra += size - unit.go_len();
        units.push((after, extra));
    }
    units
}

/// The start of `text` in `file` when Go `stringTable.add` finds it there
/// for a node of kind `kind` at `pos`..`end` (Go offsets): the slice that
/// ends at `end` (before the closing quote of a string literal or template
/// tail). `None` when it does not.
#[inline]
fn file_slice_start(
    file: &[u8],
    text: &[u8],
    kind: SyntaxKind,
    pos: i64,
    end: i64,
) -> Option<usize> {
    if end - pos <= 0 || end > file.len() as i64 {
        return None;
    }
    let end_offset = i64::from(
        kind == SyntaxKind::StringLiteral
            || kind == SyntaxKind::TemplateTail
            || kind == SyntaxKind::NoSubstitutionTemplateLiteral,
    );
    let end = usize::try_from(end - end_offset).ok()?;
    let start = end.checked_sub(text.len())?;
    (file[start..end] == *text).then_some(start)
}

impl StringTable {
    /// The Go bytes of the file text.
    fn file_bytes(&self) -> &[u8] {
        self.go_file_bytes
            .as_deref()
            .unwrap_or_else(|| self.file_text.as_bytes())
    }

    /// PORT: the Go byte offset of the port offset `pos` in the file text.
    /// `pos` must not be inside a unit. A negative offset is kept.
    fn go_offset(&self, pos: i32) -> i64 {
        if pos <= 0 {
            return pos as i64;
        }
        let pos = pos as usize;
        let n = self.units.partition_point(|&(after, _)| after <= pos);
        let extra = n.checked_sub(1).map_or(0, |i| self.units[i].1);
        (pos - extra) as i64
    }

    // Go: api/encoder/stringtable.go:25 (*stringTable).add
    pub fn add(&mut self, text: &str, kind: SyntaxKind, pos: i32, end: i32) -> u32 {
        let index = self.offsets.len() as u32;
        // PORT: Go offsets and Go bytes (see the module comment). i64 keeps a
        // negative `end` below the text length, as Go `int` does.
        let (pos, mut end) = (self.go_offset(pos), self.go_offset(end));
        if kind == SyntaxKind::SourceFile {
            self.offsets.push(pos as u32);
            self.offsets.push(end as u32);
            return index;
        }
        // PERF: (apiperf2) a file text with no `GO_STRING_MARKER` has its
        // own bytes as Go bytes (`go_file_bytes` is `None`). A text equal to
        // its slice of that file then has no marker either, so its Go bytes
        // are its bytes and the match below finds the same slice: no
        // marker scan (`go_string_bytes`) for it.
        if self.go_file_bytes.is_none()
            && let Some(start) =
                file_slice_start(self.file_text.as_bytes(), text.as_bytes(), kind, pos, end)
        {
            self.offsets.push(start as u32);
            self.offsets.push((start + text.len()) as u32);
            return index;
        }
        let text = go_string_bytes(text);
        let length = text.len() as i64;
        if end - pos > 0 && end <= self.file_bytes().len() as i64 {
            // pos includes leading trivia, but we can usually infer the actual start of the
            // string from the kind and end
            let mut end_offset: i64 = 0;
            if kind == SyntaxKind::StringLiteral
                || kind == SyntaxKind::TemplateTail
                || kind == SyntaxKind::NoSubstitutionTemplateLiteral
            {
                end_offset = 1;
            }
            end -= end_offset;
            let start = end - length;
            // PORT: Go `t.fileText[start:end]` panics when `start` is negative;
            // the `usize` conversion makes the Rust slice panic too.
            let file_slice = &self.file_bytes()[start as usize..end as usize];
            if *file_slice == *text {
                self.offsets.push(start as u32);
                self.offsets.push(end as u32);
                return index;
            }
        }
        // no exact match, so we need to add it to the string table
        let offset = self.file_bytes().len() + self.other_strings.len();
        self.other_strings.extend_from_slice(&text);
        self.offsets.push(offset as u32);
        self.offsets.push((offset + length as usize) as u32);
        index
    }

    // Go: api/encoder/stringtable.go:54 (*stringTable).encode
    pub fn encode(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.encoded_length());
        append_uint32s(&mut result, &self.offsets);
        result.extend_from_slice(self.file_bytes());
        result.extend_from_slice(&self.other_strings);
        result
    }

    // Go: api/encoder/stringtable.go:62 (*stringTable).stringLength
    pub fn string_length(&self) -> usize {
        self.file_bytes().len() + self.other_strings.len()
    }

    // Go: api/encoder/stringtable.go:66 (*stringTable).encodedLength
    pub fn encoded_length(&self) -> usize {
        self.offsets.len() * 4 + self.file_bytes().len() + self.other_strings.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string data section of `table.encode()`.
    fn string_data(table: &StringTable) -> Vec<u8> {
        table.encode()[table.offsets.len() * 4..].to_vec()
    }

    // The native-preview API test "unicode escapes" has the file
    // `"\ud800a\udc00"`. Go stores the value in 7 bytes (two 3-byte WTF-8
    // surrogates and "a"), after the 15 bytes of the file text.
    #[test]
    fn lone_surrogates_are_wtf8() {
        let file_text = r#""\ud800a\udc00""#;
        let mut table = new_string_table(file_text, 2);
        let text = encode_js_string_rune(0xD800) + "a" + &encode_js_string_rune(0xDC00);
        assert_eq!(table.add(&text, SyntaxKind::StringLiteral, 0, 15), 0);
        assert_eq!(table.offsets, [15, 22]);
        let mut want = file_text.as_bytes().to_vec();
        want.extend_from_slice(b"\xED\xA0\x80a\xED\xB0\x80");
        assert_eq!(string_data(&table), want);
        assert_eq!(table.string_length(), 22);
        assert_eq!(table.encoded_length(), table.encode().len());
    }

    // A file with an invalid byte and a real U+FDD0. Go writes their 1 and
    // 3 bytes, and a string after them points into the file at Go offsets.
    #[test]
    fn file_units_use_go_offsets() {
        let file_text: &'static str = go_string_from_bytes(b"\xFF\xEF\xB7\x90 ab".to_vec()).leak();
        let mut table = new_string_table(file_text, 3);
        let port_end = file_text.len() as i32;
        assert_eq!(port_end, 7 + 6 + 3);
        assert_eq!(table.add(file_text, SyntaxKind::SourceFile, 0, port_end), 0);
        assert_eq!(table.add("ab", SyntaxKind::Identifier, 13, port_end), 2);
        // "\u{FDD0}" has no match in the file text, so it goes after it.
        let fdd0 = go_string_from_bytes(b"\xEF\xB7\x90".to_vec());
        assert_eq!(table.add(&fdd0, SyntaxKind::Unknown, 0, 0), 4);
        assert_eq!(table.offsets, [0, 7, 5, 7, 7, 10]);
        assert_eq!(
            string_data(&table),
            b"\xFF\xEF\xB7\x90 ab\xEF\xB7\x90".to_vec()
        );
    }
}
