use std::{ops::Range, rc::Rc};

use gpui::{AnyElement, App, Context, Font, Pixels, SharedString, Size, Window};
use ropey::Rope;

use super::{EditorState, TextDecoration};

/// A visual replacement. The source buffer, clipboard and undo history stay intact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayReplacement {
    pub range: Range<usize>,
    pub text: SharedString,
}

/// A rendered block replacing complete source lines while they are inactive.
#[derive(Clone)]
pub struct EditorDisplayBlock {
    pub range: Range<usize>,
    pub render: Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>,
    pub cache: Rc<std::cell::RefCell<EditorDisplayBlockCache>>,
}

/// Reusable measurements for a block. Keep this alive across display snapshots.
#[derive(Default)]
pub struct EditorDisplayBlockCache {
    pub(crate) key: Option<(Pixels, Font, Pixels, Pixels)>,
    pub(crate) size: Size<Pixels>,
    height_hint: Option<Rc<dyn Fn(Pixels) -> Pixels>>,
}

impl EditorDisplayBlockCache {
    /// Supply a cheap initial height for blocks whose geometry is known without
    /// constructing their view. Visible layout replaces this hint with a measurement.
    pub fn set_height_hint(&mut self, hint: impl Fn(Pixels) -> Pixels + 'static) {
        self.height_hint = Some(Rc::new(hint));
    }

    pub(crate) fn initial_size(&self, width: Pixels, line_height: Pixels) -> Option<Size<Pixels>> {
        self.height_hint
            .as_ref()
            .map(|hint| gpui::size(width, hint(line_height)))
    }
}

/// Application-owned presentation of the editor's source.
#[derive(Clone, Default)]
pub struct EditorDisplay {
    pub replacements: Vec<DisplayReplacement>,
    pub decorations: Vec<TextDecoration>,
    pub blocks: Vec<EditorDisplayBlock>,
}

/// Provides a presentation without changing the editable document.
pub trait EditorDisplayProvider {
    /// A source replacement, with byte positions in the previous snapshot.
    /// The new Rope is supplied after the replacement. Consumers can record
    /// edits here and defer analysis until the next display request.
    fn text_changed(
        &mut self,
        _old_text: &Rope,
        _text: &Rope,
        _range: &Range<usize>,
        _new_len: usize,
    ) {
    }

    /// An optional navigation target under the source caret.
    fn link_at(&self, _offset: usize) -> Option<SharedString> {
        None
    }

    /// Optional application navigation. Invoked after the input event releases
    /// its editor borrow, allowing the handler to replace the active document.
    fn link_handler(&self) -> Option<Rc<dyn Fn(&SharedString, &mut Window, &mut App)>> {
        None
    }

    fn display(
        &mut self,
        text: &Rope,
        selection: Range<usize>,
        focused: bool,
        window: &Window,
        cx: &App,
    ) -> EditorDisplay;
}

pub type SharedEditorDisplayProvider = Rc<std::cell::RefCell<dyn EditorDisplayProvider>>;

impl EditorState {
    /// Set a source-preserving presentation, or `None` to show the source.
    pub fn set_display_provider(
        &mut self,
        provider: Option<SharedEditorDisplayProvider>,
        cx: &mut Context<Self>,
    ) {
        let unchanged = match (&self.extras.display_provider, &provider) {
            (None, None) => true,
            (Some(current), Some(next)) => Rc::ptr_eq(current, next),
            _ => false,
        };
        if unchanged {
            return;
        }
        self.extras.display_provider = provider;
        cx.notify();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayBlockLayout {
    pub(crate) lines: Range<usize>,
    pub(crate) rows: usize,
}

/// Byte mappings for one projected line. Both sides use UTF-8 byte offsets.
#[derive(Clone, Debug)]
pub(crate) struct LineProjection {
    pub(crate) text: SharedString,
    source_len: usize,
    segments: Vec<(Range<usize>, Range<usize>, bool)>,
}

impl LineProjection {
    pub(crate) fn new(
        source: &str,
        start: usize,
        replacements: &[DisplayReplacement],
    ) -> Option<Self> {
        let mut text = String::new();
        let mut segments = Vec::new();
        let mut offset = 0;
        // Providers supply sorted, non-overlapping replacements. Seek directly to
        // this line instead of scanning the entire document for every line.
        let first = replacements.partition_point(|replacement| replacement.range.end <= start);
        for replacement in replacements[first..]
            .iter()
            .take_while(|replacement| replacement.range.start < start + source.len())
        {
            let from = replacement.range.start.saturating_sub(start);
            let to = (replacement.range.end - start).min(source.len());
            // Invalid application ranges must not reach the text shaper.
            if from < offset
                || from > to
                || !source.is_char_boundary(from)
                || !source.is_char_boundary(to)
                || replacement.text.contains('\n')
            {
                continue;
            }
            if from > offset {
                let display_start = text.len();
                text.push_str(&source[offset..from]);
                segments.push((offset..from, display_start..text.len(), true));
            }
            let display_start = text.len();
            text.push_str(&replacement.text);
            segments.push((from..to, display_start..text.len(), false));
            offset = to;
        }
        if segments.is_empty() {
            return None;
        }
        if offset < source.len() {
            let display_start = text.len();
            text.push_str(&source[offset..]);
            segments.push((offset..source.len(), display_start..text.len(), true));
        }
        Some(Self {
            text: text.into(),
            source_len: source.len(),
            segments,
        })
    }

    pub(crate) fn source_to_display(&self, offset: usize) -> usize {
        for (source, display, copied) in &self.segments {
            if offset <= source.end {
                if *copied {
                    return display.start + offset.saturating_sub(source.start).min(display.len());
                }
                return if offset >= source.end {
                    display.end
                } else {
                    display.start
                };
            }
        }
        self.text.len()
    }

    pub(crate) fn display_to_source(&self, offset: usize) -> usize {
        for (source, display, copied) in &self.segments {
            if !display.is_empty() && offset <= display.end {
                if *copied {
                    return source.start + offset.saturating_sub(display.start).min(source.len());
                }
                return if offset >= display.end {
                    source.end
                } else {
                    source.start
                };
            }
        }
        self.source_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concealed_unicode_text_maps_back_to_source() {
        let source = "**niño 世界**";
        let projection = LineProjection::new(
            source,
            10,
            &[
                DisplayReplacement {
                    range: 10..12,
                    text: "".into(),
                },
                DisplayReplacement {
                    range: 10 + source.len() - 2..10 + source.len(),
                    text: "".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(projection.text.as_ref(), "niño 世界");
        for (offset, _) in projection.text.char_indices() {
            assert_eq!(projection.display_to_source(offset), offset + 2);
            assert_eq!(projection.source_to_display(offset + 2), offset);
        }
        assert_eq!(
            projection.source_to_display(source.len()),
            projection.text.len()
        );
        assert_eq!(
            projection.display_to_source(projection.text.len()),
            source.len() - 2
        );
    }

    #[test]
    fn replacement_maps_multibyte_marker_boundaries() {
        let projection = LineProjection::new(
            "- item",
            0,
            &[DisplayReplacement {
                range: 0..1,
                text: "•".into(),
            }],
        )
        .unwrap();
        assert_eq!(projection.text.as_ref(), "• item");
        assert_eq!(projection.display_to_source(3), 1);
        assert_eq!(projection.display_to_source(4), 2);
        assert_eq!(projection.source_to_display(2), 4);
    }

    #[test]
    fn invalid_utf8_ranges_are_ignored() {
        assert!(
            LineProjection::new(
                "世界",
                0,
                &[DisplayReplacement {
                    range: 1..3,
                    text: "".into()
                },]
            )
            .is_none()
        );
    }

    #[test]
    fn entities_preserve_surrounding_source_positions() {
        let source = "prefix &amp; niño 世界";
        let projection = LineProjection::new(
            source,
            0,
            &[DisplayReplacement {
                range: 7..12,
                text: "&".into(),
            }],
        )
        .unwrap();
        assert_eq!(projection.text.as_ref(), "prefix & niño 世界");
        for (offset, _) in source
            .char_indices()
            .filter(|(offset, _)| *offset < 7 || *offset >= 12)
        {
            let displayed = projection.source_to_display(offset);
            assert_eq!(projection.display_to_source(displayed), offset);
        }
    }

    #[test]
    #[ignore = "manual projection scaling measurement"]
    fn projection_scale_benchmark() {
        for lines in [1_000, 5_000, 10_000] {
            let replacements = (0..lines)
                .flat_map(|line| {
                    let start = line * 9;
                    [
                        DisplayReplacement {
                            range: start..start + 2,
                            text: "".into(),
                        },
                        DisplayReplacement {
                            range: start + 6..start + 8,
                            text: "".into(),
                        },
                    ]
                })
                .collect::<Vec<_>>();
            let start = std::time::Instant::now();
            for line in 0..lines {
                let projection = LineProjection::new("**text**", line * 9, &replacements).unwrap();
                assert_eq!(projection.text.as_ref(), "text");
                std::hint::black_box(projection);
            }
            eprintln!(
                "projection lines={lines}, total_ms={:.2}",
                start.elapsed().as_secs_f64() * 1000.
            );
        }
    }
}
