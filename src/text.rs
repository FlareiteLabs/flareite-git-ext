//! Texts of open documents, and the editor's positions in them, for diff
//! and merge tabs.
//!
//! The extension API counts columns in UTF-8 bytes; `flareite-diff` counts
//! them in UTF-16 code units, as the editor's workbench does. Ranges sent to
//! the editor are converted here.

use std::ops::Range;

use flareite_extension_api::host;
use flareite_extension_api::types::{Position, Range as ApiRange, TextEdit};

/// Most lines read at once (the API reads at most 10,000).
const CHUNK: u32 = 5_000;

/// An open document's whole text (lines joined with `\n`) and its version.
pub fn document_text(id: u64) -> Option<(String, u64)> {
  let info = host::document(id).ok()?;
  let mut lines: Vec<String> = Vec::with_capacity(info.line_count as usize);
  let mut version = info.version;
  let mut start = 0;
  while start < info.line_count {
    let end = (start + CHUNK).min(info.line_count);
    let slice = host::read_lines(id, start, end).ok()?;
    if slice.lines.is_empty() {
      break;
    }
    if start > 0 && slice.version != version {
      // Edited while being read: read it again.
      return document_text(id);
    }
    version = slice.version;
    start += u32::try_from(slice.lines.len()).unwrap_or(u32::MAX);
    lines.extend(slice.lines);
  }
  Some((lines.join("\n"), version))
}

/// A text's lines, without their line breaks (as `flareite-diff` splits
/// them).
pub fn lines(text: &str) -> Vec<&str> {
  text
    .split('\n')
    .map(|line| line.strip_suffix('\r').unwrap_or(line))
    .collect()
}

/// The UTF-8 column of UTF-16 column `column` in `line`.
pub fn utf8_column(line: &str, column: usize) -> u32 {
  let mut units = 0;
  for (index, c) in line.char_indices() {
    if units >= column {
      return u32::try_from(index).unwrap_or(u32::MAX);
    }
    units += c.len_utf16();
  }
  u32::try_from(line.len()).unwrap_or(u32::MAX)
}

/// The API's position of `line` and UTF-16 `column`, in a text's `lines`.
pub fn position(lines: &[&str], line: usize, column: usize) -> Position {
  Position {
    line: u32::try_from(line).unwrap_or(u32::MAX),
    column: lines.get(line).map_or(0, |text| utf8_column(text, column)),
  }
}

/// Whole lines `lines` (`[start, end)`), as a range for whole-line
/// decorations: from the start of the first to the start of the one after.
pub fn whole_lines(lines: &Range<usize>) -> ApiRange {
  let at = |line: usize| Position {
    line: u32::try_from(line).unwrap_or(u32::MAX),
    column: 0,
  };
  ApiRange {
    start: at(lines.start),
    end: at(lines.end),
  }
}

/// The API's position of byte `offset` in `text`.
pub fn offset_position(text: &str, offset: usize) -> Position {
  let before = &text[..offset.min(text.len())];
  let line = before.matches('\n').count();
  let start = before.rfind('\n').map_or(0, |at| at + 1);
  Position {
    line: u32::try_from(line).unwrap_or(u32::MAX),
    column: u32::try_from(before.len() - start).unwrap_or(u32::MAX),
  }
}

/// Edits replacing byte ranges of `text` (as `flareite_diff::Splice`s give
/// them), in the API's positions.
pub fn edits(text: &str, splices: &[flareite_diff::Splice]) -> Vec<TextEdit> {
  splices
    .iter()
    .map(|splice| TextEdit {
      range: ApiRange {
        start: offset_position(text, splice.range.start),
        end: offset_position(text, splice.range.end),
      },
      text: splice.text.clone(),
    })
    .collect()
}

/// `parts` as a JSON array of strings: a tab's key.
pub fn encode_key(parts: &[&str]) -> String {
  let mut key = String::from("[");
  for (index, part) in parts.iter().enumerate() {
    if index > 0 {
      key.push(',');
    }
    key.push('"');
    for c in part.chars() {
      match c {
        '"' => key.push_str("\\\""),
        '\\' => key.push_str("\\\\"),
        c if c.is_control() => key.push_str(&format!("\\u{:04x}", u32::from(c))),
        c => key.push(c),
      }
    }
    key.push('"');
  }
  key.push(']');
  key
}

/// The strings of a key [`encode_key`] made; `None` if it isn't one.
pub fn decode_key(key: &str) -> Option<Vec<String>> {
  let inner = key.strip_prefix('[')?.strip_suffix(']')?;
  let mut parts = Vec::new();
  let mut chars = inner.chars().peekable();
  while chars.peek().is_some() {
    if chars.next()? != '"' {
      return None;
    }
    let mut part = String::new();
    loop {
      match chars.next()? {
        '"' => break,
        '\\' => match chars.next()? {
          'u' => {
            let hex: String = (0..4).filter_map(|_| chars.next()).collect();
            part.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
          }
          other => part.push(other),
        },
        c => part.push(c),
      }
    }
    parts.push(part);
    match chars.next() {
      None => break,
      Some(',') => {}
      Some(_) => return None,
    }
  }
  Some(parts)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keys_hold_any_text() {
    let parts = [
      "git::src/a \"b\".rs",
      "C:\\repo\\a.rs",
      "a.rs (Working Tree)",
    ];
    let key = encode_key(&parts);
    assert_eq!(decode_key(&key), Some(parts.map(String::from).to_vec()));
    assert_eq!(decode_key("[]"), Some(Vec::new()));
    assert_eq!(decode_key("nope"), None);
  }

  #[test]
  fn columns_are_converted_to_bytes() {
    // "é" is one UTF-16 unit and two bytes; "😀" two units and four bytes.
    assert_eq!(utf8_column("aé😀b", 2), 3);
    assert_eq!(utf8_column("aé😀b", 4), 7);
    assert_eq!(utf8_column("ab", 9), 2);
  }

  #[test]
  fn offsets_are_positions() {
    let text = "ab\ncé\nd";
    assert_eq!(offset_position(text, 0), Position { line: 0, column: 0 });
    assert_eq!(offset_position(text, 4), Position { line: 1, column: 1 });
    assert_eq!(offset_position(text, 7), Position { line: 2, column: 0 });
  }
}
