//! Conservative incremental GFM parsing shared by the editor and TextView.
//!
//! Root blocks carry positions relative to themselves, so moving a suffix only
//! changes its byte anchors. A local parse includes an unchanged block on each
//! side. Those blocks must match exactly before we reuse anything outside the
//! window. Definitions and footnotes use the full parser: their effects are not
//! confined to neighboring blocks. An unstable boundary also uses the full parser.

use std::{ops::Range, sync::Arc};

use markdown::{ParseOptions, mdast::Node};

// Avoid an expensive speculative parse followed by a full parse for huge blocks.
const MAX_WINDOW_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
struct RootBlock {
    range: Range<usize>,
    line_start: usize,
    line_end: usize,
    node: Arc<Node>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MarkdownIndex {
    blocks: Vec<RootBlock>,
    len: usize,
    global_dependencies: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct MarkdownEdit {
    /// Byte range in the previous snapshot.
    pub range: Range<usize>,
    pub new_len: usize,
}

pub(crate) struct ReparsedMarkdown {
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    /// Indices of the old root nodes replaced by `root`.
    pub old_blocks: Range<usize>,
    /// Origin of the fragment AST (which also includes boundary context).
    pub offset: usize,
    pub source: String,
    pub root: Node,
}

fn has_global_dependencies(node: &Node) -> bool {
    matches!(
        node,
        Node::Definition(_) | Node::FootnoteDefinition(_) | Node::FootnoteReference(_)
    ) || node
        .children()
        .is_some_and(|children| children.iter().any(has_global_dependencies))
}

fn relative_positions(node: &mut Node, byte: usize, line: usize, column: usize) {
    if let Some(position) = node.position_mut() {
        for point in [&mut position.start, &mut position.end] {
            point.offset -= byte;
            if point.line == line {
                point.column -= column - 1;
            }
            point.line -= line - 1;
        }
    }
    for child in node.children_mut().into_iter().flatten() {
        relative_positions(child, byte, line, column);
    }
}

fn root_blocks(source: &str, root: &Node, offset: usize) -> Option<Vec<RootBlock>> {
    root.children()?
        .iter()
        .map(|node| {
            let position = node.position()?;
            let start = position.start.offset;
            let mut relative = node.clone();
            relative_positions(
                &mut relative,
                start,
                position.start.line,
                position.start.column,
            );
            Some(RootBlock {
                range: offset + start..offset + position.end.offset,
                line_start: offset + source[..start].rfind('\n').map_or(0, |ix| ix + 1),
                // Live blocks/tasks cover physical lines, including any spaces
                // outside the AST span. Editing those spaces must rebuild them.
                line_end: offset
                    + source[position.end.offset..]
                        .find('\n')
                        .map_or(source.len(), |ix| position.end.offset + ix),
                node: Arc::new(relative),
            })
        })
        .collect()
}

impl MarkdownIndex {
    pub fn new(source: &str, root: &Node) -> Self {
        Self {
            blocks: root_blocks(source, root, 0).unwrap_or_default(),
            len: source.len(),
            // A lone CR is a Markdown line ending but not an editor LF line.
            global_dependencies: has_global_dependencies(root)
                || source
                    .as_bytes()
                    .windows(2)
                    .any(|pair| pair[0] == b'\r' && pair[1] != b'\n')
                || source.ends_with('\r'),
        }
    }

    /// `slice` reads only the planned window from the new snapshot. Callers
    /// supplying a known edit must ensure it follows this index's snapshot.
    pub fn reparse(
        &mut self,
        new_len: usize,
        edit: &MarkdownEdit,
        slice: impl FnOnce(Range<usize>) -> String,
        options: &ParseOptions,
    ) -> Option<ReparsedMarkdown> {
        if self.global_dependencies || self.blocks.is_empty()
            || edit.range.start > edit.range.end || edit.range.end > self.len
            || self.len.checked_sub(edit.range.len())?.checked_add(edit.new_len)? != new_len
            // MDX has module-wide constructs; it keeps its existing full parser.
            || options.constructs.mdx_esm || options.constructs.mdx_expression_flow
            || options.constructs.mdx_jsx_flow
        {
            return None;
        }
        // Include blocks touching the edit's boundaries, including insertions
        // in a gap. Include one additional neighbor on either side as a guard.
        let affected_start = self
            .blocks
            .partition_point(|block| block.range.end < edit.range.start)
            .min(self.blocks.len() - 1);
        let affected_end = self
            .blocks
            .partition_point(|block| block.range.start <= edit.range.end)
            .max(affected_start + 1)
            .min(self.blocks.len());
        let first = affected_start.saturating_sub(1);
        let end = (affected_end + 1).min(self.blocks.len());
        let old_range = if first == 0 {
            0
        } else {
            self.blocks[first].line_start
        }..if end == self.blocks.len() {
            self.len
        } else {
            self.blocks[end].line_start
        };
        let delta = edit.new_len as isize - edit.range.len() as isize;
        let new_range = old_range.start..old_range.end.checked_add_signed(delta)?;
        if edit.range.start < old_range.start
            || edit.range.end > old_range.end
            || old_range.len() > MAX_WINDOW_BYTES
            || new_range.len() > MAX_WINDOW_BYTES
        {
            return None;
        }
        let source = slice(new_range.clone());
        let mut root = super::wiki::parse(&source, options).ok()?;
        if has_global_dependencies(&root)
            || source.contains('\r') && source.replace("\r\n", "").contains('\r')
        {
            return None;
        }
        let blocks = root_blocks(&source, &root, new_range.start)?;
        // Exact AST equality includes child positions relative to the root
        // block, not just its type or its visible text.
        if first > 0 {
            let old = &self.blocks[first];
            let new = blocks.first()?;
            if old.line_end > edit.range.start || new != old {
                return None;
            }
        }
        if end < self.blocks.len() {
            let old = &self.blocks[end - 1];
            let new = blocks.last()?;
            if old.range.start < edit.range.end
                || old.node != new.node
                || new.range
                    != (old.range.start.checked_add_signed(delta)?
                        ..old.range.end.checked_add_signed(delta)?)
                || new.line_start != old.line_start.checked_add_signed(delta)?
                || new.line_end != old.line_end.checked_add_signed(delta)?
            {
                return None;
            }
        }
        // Keep the guard blocks themselves, including their render data. A
        // nearby diagram or formula need not be converted again while typing.
        let old = &self.blocks[first..end];
        let prefix = old
            .iter()
            .zip(&blocks)
            .take_while(|(a, b)| a.line_end <= edit.range.start && a == b)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(blocks[prefix..].iter().rev())
            .take_while(|(a, b)| {
                a.range.start >= edit.range.end
                    && a.node == b.node
                    && b.range.start == a.range.start.checked_add_signed(delta).unwrap()
                    && b.range.end == a.range.end.checked_add_signed(delta).unwrap()
                    && b.line_start == a.line_start.checked_add_signed(delta).unwrap()
                    && b.line_end == a.line_end.checked_add_signed(delta).unwrap()
            })
            .count();
        let replaced_old = if prefix == 0 {
            old_range.start
        } else {
            old[prefix - 1].range.end
        }..if suffix == 0 {
            old_range.end
        } else {
            old[old.len() - suffix].line_start
        };
        let replaced_new = if prefix == 0 {
            new_range.start
        } else {
            blocks[prefix - 1].range.end
        }..if suffix == 0 {
            new_range.end
        } else {
            blocks[blocks.len() - suffix].line_start
        };
        let children = root.children_mut()?;
        children.truncate(children.len() - suffix);
        children.drain(..prefix);
        for block in &mut self.blocks[end..] {
            block.range.start = block.range.start.checked_add_signed(delta)?;
            block.range.end = block.range.end.checked_add_signed(delta)?;
            block.line_start = block.line_start.checked_add_signed(delta)?;
            block.line_end = block.line_end.checked_add_signed(delta)?;
        }
        self.blocks.splice(first..end, blocks);
        self.len = new_len;
        Some(ReparsedMarkdown {
            old_range: replaced_old,
            new_range: replaced_new,
            old_blocks: first + prefix..end - suffix,
            offset: new_range.start,
            source,
            root,
        })
    }
}

/// For consumers receiving whole strings rather than editor edit events. This
/// discovers the change in linear time without analyzing the unchanged text.
pub(crate) fn edit_between(old: &str, new: &str) -> MarkdownEdit {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = old[start..]
        .bytes()
        .rev()
        .zip(new[start..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    MarkdownEdit {
        range: start..old.len() - suffix,
        new_len: new.len() - suffix - start,
    }
}
