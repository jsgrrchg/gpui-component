use std::ops::Range;

use ropey::Rope;

use crate::input::RopeExt as _;

fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn quote_prefix(line: &str) -> (usize, Option<usize>) {
    let bytes = line.as_bytes();
    let mut end = 0;
    let mut last_quote = None;
    while bytes.get(end).is_some_and(|byte| whitespace(*byte)) {
        end += 1;
    }
    while bytes.get(end) == Some(&b'>') {
        last_quote = Some(end);
        end += 1;
        while bytes.get(end).is_some_and(|byte| whitespace(*byte)) {
            end += 1;
        }
    }
    (end, last_quote)
}

fn inside_fence(text: &Rope, row: usize) -> bool {
    let mut fence = None::<(u8, usize)>;
    for line in text.iter_lines().take(row) {
        let line = line.to_string();
        let (prefix, _) = quote_prefix(&line);
        let content = &line[prefix..];
        let Some(marker @ (b'`' | b'~')) = content.as_bytes().first().copied() else {
            continue;
        };
        let count = content.bytes().take_while(|byte| *byte == marker).count();
        if count < 3 {
            continue;
        }
        if let Some((open, length)) = fence {
            if open == marker && count >= length && content[count..].trim().is_empty() {
                fence = None;
            }
        } else if marker != b'`' || !content[count..].contains('`') {
            fence = Some((marker, count));
        }
    }
    fence.is_some()
}

/// Continue Markdown list, task, and quote prefixes in a single undoable edit.
/// An empty item exits its list; a new task always starts unchecked.
pub(super) fn newline_edit(text: &Rope, selection: Range<usize>) -> Option<(Range<usize>, String)> {
    let point = text.offset_to_point(selection.start);
    if inside_fence(text, point.row) {
        return None;
    }
    let line = text.slice_line(point.row).to_string();
    let bytes = line.as_bytes();
    let (prefix, last_quote) = quote_prefix(&line);
    let content = &line[prefix..];
    let mut end = prefix;
    let marker = match bytes.get(prefix).copied() {
        Some(marker @ (b'-' | b'+' | b'*'))
            if bytes.get(prefix + 1).is_some_and(|byte| whitespace(*byte)) =>
        {
            // A thematic break is not a list item.
            if content
                .bytes()
                .filter(|byte| !whitespace(*byte))
                .all(|byte| byte == marker)
                && content.bytes().filter(|byte| *byte == marker).count() >= 3
            {
                return None;
            }
            end += 1;
            Some((marker as char).to_string())
        }
        Some(b'0'..=b'9') => {
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end - prefix <= 9
                && matches!(bytes.get(end), Some(b'.' | b')'))
                && bytes.get(end + 1).is_some_and(|byte| whitespace(*byte))
            {
                let number = line[prefix..end].parse::<u32>().ok()? + 1;
                let marker = format!("{number}{}", bytes[end] as char);
                end += 1;
                Some(marker)
            } else {
                None
            }
        }
        _ => None,
    };

    let start = text.line_start_offset(point.row);
    if let Some(marker) = marker {
        while bytes.get(end).is_some_and(|byte| whitespace(*byte)) {
            end += 1;
        }
        let task = bytes.get(end) == Some(&b'[')
            && matches!(bytes.get(end + 1), Some(b' ' | b'x' | b'X'))
            && bytes.get(end + 2) == Some(&b']')
            && bytes.get(end + 3).is_none_or(|byte| whitespace(*byte));
        if task {
            end += 3;
            while bytes.get(end).is_some_and(|byte| whitespace(*byte)) {
                end += 1;
            }
        }
        if point.column < end {
            return None;
        }
        if line[end..].trim().is_empty() && selection.is_empty() {
            return Some((start + prefix..start + line.len(), String::new()));
        }
        let task_marker = if task { "[ ] " } else { "" };
        Some((
            selection,
            format!("\n{}{marker} {task_marker}", &line[..prefix]),
        ))
    } else if let Some(last_quote) = last_quote {
        if point.column < prefix {
            return None;
        }
        if content.trim().is_empty() && selection.is_empty() {
            Some((start + last_quote..start + line.len(), String::new()))
        } else {
            Some((selection, format!("\n{}", &line[..prefix])))
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enter(source: &str) -> String {
        let mut text = Rope::from(source);
        let (range, inserted) = newline_edit(&text, text.len()..text.len()).unwrap();
        text.replace(range, &inserted);
        text.to_string()
    }

    #[test]
    fn lists_tasks_and_quotes_continue() {
        for (source, expected) in [
            ("- item", "- item\n- "),
            ("* item", "* item\n* "),
            ("+ item", "+ item\n+ "),
            ("12. item", "12. item\n13. "),
            ("9) item", "9) item\n10) "),
            ("  - niño 世界", "  - niño 世界\n  - "),
            ("- [x] done", "- [x] done\n- [ ] "),
            ("> - [X] done", "> - [X] done\n> - [ ] "),
            ("> paragraph", "> paragraph\n> "),
            ("> > paragraph", "> > paragraph\n> > "),
        ] {
            assert_eq!(enter(source), expected, "{source:?}");
        }
    }

    #[test]
    fn empty_items_exit_and_empty_quotes_remove_one_level() {
        assert_eq!(enter("- item\n- "), "- item\n");
        assert_eq!(enter("- [ ] "), "");
        assert_eq!(enter("> - [ ] "), "> ");
        assert_eq!(enter("> > "), "> ");
    }

    #[test]
    fn code_and_thematic_breaks_are_not_continued() {
        for source in [
            "```\n- literal",
            "~~~rust\n1. literal",
            "- - -",
            "* * *",
            "ordinary text",
        ] {
            let text = Rope::from(source);
            assert!(
                newline_edit(&text, text.len()..text.len()).is_none(),
                "{source:?}"
            );
        }
        assert_eq!(
            enter("```\ncode\n```\n- item"),
            "```\ncode\n```\n- item\n- "
        );
    }
}
