//! Native, offline rendering for Mermaid diagrams and LaTeX math expressions.
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, OnceLock},
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, Image,
    ImageFormat, ImageSource, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    MouseButton, ObjectFit, ParentElement as _, Pixels, Rgba, SharedString, Styled as _,
    StyledImage as _, Window, div, img, px, size,
};
use markdown::mdast::Node;
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::{parse_node::ParseNode, parser::parse};
use ratex_types::{color::Color, math_style::MathStyle};

use super::{MarkdownNode, MarkdownParseContext};
use crate::{ActiveTheme as _, clipboard::Clipboard};

const MAX_SOURCE_BYTES: usize = 32 * 1024;
const NAME: &str = "markdown-advanced-content";

pub(crate) fn parse_options() -> markdown::ParseOptions {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_flow = true;
    options.constructs.math_text = true;
    options
}

pub(crate) fn is_advanced_fence(language: Option<&str>) -> bool {
    language
        .and_then(|language| language.split_whitespace().next())
        .is_some_and(|language| {
            ["mermaid", "math", "latex", "tex"]
                .iter()
                .any(|name| language.eq_ignore_ascii_case(name))
        })
}

pub(crate) fn contains_inline_math(node: &Node) -> bool {
    matches!(node, Node::InlineMath(_))
        || node
            .children()
            .is_some_and(|children| children.iter().any(contains_inline_math))
}

#[derive(Clone)]
struct RenderedSvg {
    image: Arc<Image>,
    width: f32,
    height: f32,
}

impl RenderedSvg {
    fn new(mut svg: String, width: f32, height: f32) -> Self {
        // GPUI decodes SVGs at twice their root size. Keep the raster bounded
        // while preserving the original viewBox and logical aspect ratio.
        let scale = (2048. / width).min(2048. / height).min(1.);
        for (attribute, value) in [("width", width * scale), ("height", height * scale)] {
            let header_end = svg.find('>').unwrap_or(0);
            let pattern = format!(" {attribute}=\"");
            if let Some(start) = svg[..header_end].find(&pattern) {
                let start = start + pattern.len();
                if let Some(end) = svg[start..header_end].find('"') {
                    svg.replace_range(start..start + end, &format!("{}px", value.max(1.)));
                }
            }
        }
        Self {
            image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
            width,
            height,
        }
    }
}

/// Measure from the SVG's geometry before its raster asset finishes loading.
/// Explicit image dimensions keep async decoding from changing reserved rows.
struct SvgFigure(RenderedSvg);

impl IntoElement for SvgFigure {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SvgFigure {
    type RequestLayoutState = ();
    type PrepaintState = AnyElement;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        let intrinsic = size(px(self.0.width), px(self.0.height));
        let layout =
            window.request_measured_layout(Default::default(), move |known, available, _, _| {
                let width = known
                    .width
                    .or_else(|| match available.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                    .unwrap_or(intrinsic.width)
                    .min(intrinsic.width)
                    .max(px(1.));
                size(width, intrinsic.height * (width / intrinsic.width))
            });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let mut image = img(self.0.image.clone())
            .id("advanced-svg-image")
            .object_fit(ObjectFit::Contain)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .into_any_element();
        image.prepaint_as_root(
            bounds.origin,
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            window,
            cx,
        );
        image
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        image: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        image.paint(window, cx);
    }
}

fn guarded<T>(
    source: &str,
    render: impl FnOnce() -> Result<T, SharedString>,
) -> Result<T, SharedString> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("Content exceeds the 32 KiB rendering limit".into());
    }
    catch_unwind(AssertUnwindSafe(render))
        .unwrap_or_else(|_| Err("The renderer could not process this content".into()))
}

/// Reusable parsed formula. Geometry and SVGs survive repeated layouts; the
/// small color cache also allows simultaneous views with different themes.
pub(crate) struct MathFormula {
    source: SharedString,
    ast: Vec<ParseNode>,
    display: bool,
    pub width: f32,
    pub height: f32,
    images: Mutex<VecDeque<(Rgba, Result<RenderedSvg, SharedString>)>>,
}

impl std::fmt::Debug for MathFormula {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MathFormula")
            .field("source", &self.source)
            .finish()
    }
}

impl PartialEq for MathFormula {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.display == other.display
    }
}

impl MathFormula {
    pub fn parse(source: &str, display: bool) -> Result<Arc<Self>, SharedString> {
        guarded(source, || {
            let ast = parse(source).map_err(|error| SharedString::from(error.to_string()))?;
            let style = if display {
                MathStyle::Display
            } else {
                MathStyle::Text
            };
            let list = to_display_list(&layout(&ast, &LayoutOptions::default().with_style(style)));
            let font_size = if display { 20. } else { 16. };
            let width = (list.width * font_size + 2.) as f32;
            let height = (list.total_height() * font_size + 2.) as f32;
            if !width.is_finite() || !height.is_finite() || width > 16384. || height > 16384. {
                return Err("Formula dimensions are too large".into());
            }
            Ok(Arc::new(Self {
                source: source.to_string().into(),
                ast,
                display,
                width: width.max(1.),
                height: height.max(1.),
                images: Mutex::new(VecDeque::new()),
            }))
        })
    }

    fn rendered(&self, foreground: Rgba) -> Result<RenderedSvg, SharedString> {
        let mut cache = self
            .images
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((_, image)) = cache.iter().find(|(color, _)| *color == foreground) {
            return image.clone();
        }
        let rendered = guarded(&self.source, || {
            let style = if self.display {
                MathStyle::Display
            } else {
                MathStyle::Text
            };
            let options = LayoutOptions::default()
                .with_style(style)
                .with_color(Color::new(
                    foreground.r,
                    foreground.g,
                    foreground.b,
                    foreground.a,
                ));
            let list = to_display_list(&layout(&self.ast, &options));
            let svg = ratex_svg::render_to_svg_with_color_syntax(
                &list,
                &ratex_svg::SvgOptions {
                    font_size: if self.display { 20. } else { 16. },
                    padding: 1.,
                    stroke_width: 0.6,
                    embed_glyphs: true,
                    ..Default::default()
                },
                ratex_svg::SvgColorSyntax::Rgb,
            );
            Ok(RenderedSvg::new(svg, self.width, self.height))
        });
        if cache.len() >= 4 {
            cache.pop_front();
        }
        cache.push_back((foreground, rendered.clone()));
        rendered
    }

    pub fn image_source(&self, cx: &App) -> ImageSource {
        match self.rendered(cx.theme().foreground.into()) {
            Ok(svg) => svg.image.into(),
            Err(_) => {
                // The formula's source remains available through editing and
                // source copying even if a backend fails after parsing it.
                let source = self
                    .source
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                let svg = format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\"><text x=\"1\" y=\"16\" font-family=\"monospace\" font-size=\"14\" fill=\"#e5484d\">{source}</text></svg>",
                    self.width,
                    self.height.max(20.),
                    self.width,
                    self.height.max(20.)
                );
                RenderedSvg::new(svg, self.width, self.height.max(20.))
                    .image
                    .into()
            }
        }
    }
}

struct MermaidDiagram {
    source: SharedString,
    light: OnceLock<Result<RenderedSvg, SharedString>>,
    dark: OnceLock<Result<RenderedSvg, SharedString>>,
}

impl MermaidDiagram {
    fn rendered(&self, dark: bool) -> Result<RenderedSvg, SharedString> {
        let cache = if dark { &self.dark } else { &self.light };
        cache
            .get_or_init(|| {
                guarded(&self.source, || {
                    let mut options = mermaid_rs_renderer::RenderOptions::default();
                    options.theme = if dark {
                        mermaid_rs_renderer::Theme::dark()
                    } else {
                        mermaid_rs_renderer::Theme::modern()
                    };
                    options.theme.font_family = "DejaVu Sans, Arial, sans-serif".into();
                    let svg = mermaid_rs_renderer::render_with_options(&self.source, options)
                        .map_err(|error| SharedString::from(error.to_string()))?;
                    let viewbox = svg
                        .split_once("viewBox=\"")
                        .and_then(|(_, rest)| rest.split_once('"'))
                        .map(|(value, _)| value)
                        .ok_or_else(|| SharedString::from("Diagram has no SVG viewBox"))?;
                    let dimensions: Vec<f32> = viewbox
                        .split_whitespace()
                        .filter_map(|number| number.parse().ok())
                        .collect();
                    let [_, _, width, height] = dimensions.as_slice() else {
                        return Err("Invalid diagram dimensions".into());
                    };
                    if !width.is_finite()
                        || !height.is_finite()
                        || *width <= 0.
                        || *height <= 0.
                        || *width > 16384.
                        || *height > 16384.
                    {
                        return Err("Diagram dimensions are too large".into());
                    }
                    Ok(RenderedSvg::new(svg, *width, *height))
                })
            })
            .clone()
    }
}

enum Content {
    Mermaid(MermaidDiagram),
    Math(Result<Arc<MathFormula>, SharedString>),
}

struct AdvancedBlock {
    source: SharedString,
    content: Content,
}

pub(crate) fn parse_block(node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
    let (source, mermaid) = match node {
        Node::Math(math) => (&math.value, false),
        Node::Code(code) if is_advanced_fence(code.lang.as_deref()) => (
            &code.value,
            code.lang
                .as_deref()?
                .split_whitespace()
                .next()?
                .eq_ignore_ascii_case("mermaid"),
        ),
        _ => return None,
    };
    let source: SharedString = source.clone().into();
    let content = if mermaid {
        Content::Mermaid(MermaidDiagram {
            source: source.clone(),
            light: OnceLock::new(),
            dark: OnceLock::new(),
        })
    } else {
        Content::Math(MathFormula::parse(&source, true))
    };
    Some(
        MarkdownNode::new(
            NAME,
            AdvancedBlock {
                source: source.clone(),
                content,
            },
        )
        .text(source)
        .markdown(cx.node_source(node).unwrap_or_default().to_string()),
    )
}

pub(crate) fn render_block(
    node: &MarkdownNode,
    _window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let data = node.data::<AdvancedBlock>()?;
    let (label, rendered) = match &data.content {
        Content::Mermaid(diagram) => ("Mermaid", diagram.rendered(cx.theme().is_dark())),
        Content::Math(math) => (
            "LaTeX",
            math.as_ref()
                .map_err(Clone::clone)
                .and_then(|math| math.rendered(cx.theme().foreground.into())),
        ),
    };
    let key = node.span.map_or(0, |span| span.start);
    let body = match rendered {
        Ok(svg) => div()
            .w_full()
            .min_w_0()
            .flex()
            .justify_center()
            .p_3()
            .child(SvgFigure(svg))
            .into_any_element(),
        Err(error) => div()
            .p_3()
            .text_color(cx.theme().danger)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(format!("Unable to render {label}")),
            )
            .child(div().text_sm().child(error))
            .child(
                div()
                    .mt_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .whitespace_normal()
                    .child(data.source.clone()),
            )
            .into_any_element(),
    };
    Some(
        div()
            .id(SharedString::from(format!("{NAME}-{key}")))
            .w_full()
            .min_w_0()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(10.))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .bg(cx.theme().foreground.opacity(0.035))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(label)
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Clipboard::new(SharedString::from(format!("{NAME}-{key}-copy")))
                                    .value(node.as_markdown().to_string())
                                    .tooltip("Copy Markdown"),
                            ),
                    ),
            )
            .child(body)
            .into_any_element(),
    )
}
