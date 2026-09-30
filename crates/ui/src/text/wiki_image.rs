use std::{collections::HashSet, ops::Range};

use markdown::{
    ParseOptions,
    mdast::Node,
    unist::{Point, Position},
};

/// NeverWrite/Obsidian image embeds. The original spelling remains the document.
pub(crate) struct WikiImage<'a> {
    pub target: &'a str,
    pub width: Option<u32>,
}

impl<'a> WikiImage<'a> {
    pub fn parse(source: &'a str) -> Option<Self> {
        let body = source.strip_prefix("![[")?.strip_suffix("]]")?;
        if body.contains(['\n', '\r', '[', ']']) {
            return None;
        }
        let (target, width) = if let Some((target, width)) = body.rsplit_once('|') {
            if width.is_empty() || !width.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let width = width
                .parse::<u32>()
                .ok()
                .filter(|width| *width > 0 && *width <= 65535)?;
            (target.trim(), Some(width))
        } else {
            (body.trim(), None)
        };
        (!target.is_empty()).then_some(Self { target, width })
    }
}

struct Embed {
    range: Range<usize>,
    marker: String,
}

/// Parse embeds before GFM can interpret punctuation inside their filenames.
/// Equal-byte-length placeholders preserve source offsets in every other node.
pub(crate) fn parse(
    source: &str,
    options: &ParseOptions,
) -> Result<Node, markdown::message::Message> {
    let original = markdown::to_mdast(source, options)?;
    if !source.contains("![[") {
        return Ok(original);
    }
    let mut text_ranges = Vec::new();
    let mut characters: HashSet<char> = source.chars().collect();
    collect_text_ranges(&original, &mut text_ranges, &mut characters);
    // A character absent from the source makes markers unambiguous even next
    // to entities, escapes, Unicode text, or another embed.
    let Some(marker_char) = (0xe000..=0xf8ff)
        .chain(0xf0000..=0xffffd)
        .filter_map(char::from_u32)
        .find(|ch| !characters.contains(ch))
    else {
        return Ok(original);
    };
    let mut embeds = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("![[") {
        let start = cursor + relative;
        cursor = start + 3;
        if source[..start]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count()
            % 2
            != 0
        {
            continue;
        }
        let Some(end) = source[cursor..].find("]]").map(|end| cursor + end + 2) else {
            break;
        };
        let ix = text_ranges.partition_point(|range| range.end <= start);
        if !text_ranges
            .get(ix)
            .is_some_and(|range| range.contains(&start))
            || WikiImage::parse(&source[start..end]).is_none()
        {
            continue;
        }
        embeds.push(Embed {
            range: start..end,
            marker: format!(
                "{marker_char}{}",
                "x".repeat(end - start - marker_char.len_utf8())
            ),
        });
        cursor = end;
    }
    if embeds.is_empty() {
        return Ok(original);
    }
    let mut masked = source.to_string();
    for embed in embeds.iter().rev() {
        masked.replace_range(embed.range.clone(), &embed.marker);
    }
    let mut root = markdown::to_mdast(&masked, options)?;
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(ix, _)| ix + 1))
        .collect();
    restore(&mut root, source, &line_starts, &embeds);
    Ok(root)
}

fn collect_text_ranges(
    node: &Node,
    ranges: &mut Vec<Range<usize>>,
    characters: &mut HashSet<char>,
) {
    if let Node::Text(text) = node {
        characters.extend(text.value.chars());
        if let Some(pos) = &text.position {
            ranges.push(pos.start.offset..pos.end.offset);
        }
    } else if let Some(children) = node.children() {
        for child in children {
            collect_text_ranges(child, ranges, characters);
        }
    }
}

fn position(source: &str, lines: &[usize], range: Range<usize>) -> Position {
    let point = |offset| {
        let line = lines.partition_point(|start| *start <= offset) - 1;
        Point {
            line: line + 1,
            column: source[lines[line]..offset].chars().count() + 1,
            offset,
        }
    };
    Position {
        start: point(range.start),
        end: point(range.end),
    }
}

fn restore(node: &mut Node, source: &str, lines: &[usize], embeds: &[Embed]) {
    let Some(children) = node.children_mut() else {
        return;
    };
    let mut output = Vec::with_capacity(children.len());
    for mut child in std::mem::take(children) {
        if let Node::Text(text) = &child {
            if let Some(pos) = &text.position {
                let first = embeds.partition_point(|embed| embed.range.end <= pos.start.offset);
                let mut decoded_start = 0;
                let mut source_start = pos.start.offset;
                for embed in embeds[first..]
                    .iter()
                    .take_while(|embed| embed.range.end <= pos.end.offset)
                {
                    let Some(at) = text.value[decoded_start..]
                        .find(&embed.marker)
                        .map(|at| decoded_start + at)
                    else {
                        continue;
                    };
                    if at > decoded_start {
                        output.push(Node::Text(markdown::mdast::Text {
                            value: text.value[decoded_start..at].to_string(),
                            position: Some(position(
                                source,
                                lines,
                                source_start..embed.range.start,
                            )),
                        }));
                    }
                    let image = WikiImage::parse(&source[embed.range.clone()]).unwrap();
                    output.push(Node::Image(markdown::mdast::Image {
                        url: image.target.to_string(),
                        alt: image.target.to_string(),
                        title: None,
                        position: Some(position(source, lines, embed.range.clone())),
                    }));
                    decoded_start = at + embed.marker.len();
                    source_start = embed.range.end;
                }
                if source_start != pos.start.offset {
                    if decoded_start < text.value.len() {
                        output.push(Node::Text(markdown::mdast::Text {
                            value: text.value[decoded_start..].to_string(),
                            position: Some(position(source, lines, source_start..pos.end.offset)),
                        }));
                    }
                    continue;
                }
            }
        }
        restore(&mut child, source, lines, embeds);
        output.push(child);
    }
    *children = output;
}
