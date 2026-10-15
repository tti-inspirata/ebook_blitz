use anyrender::PaintScene;
use blitz_dom::{BaseDocument, NodeId, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, BezPath, Cap, Circle, Insets, Rect, Stroke, Vec2};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::collections::HashMap;
use style::computed_values::direction::T as Direction;
use style::properties::generated::longhands::text_decoration_style::computed_value::T as TextDecorationStyle;
use style::values::computed::{
    BorderCornerRadius, BorderStyle, CSSPixelLength, Length, LengthPercentage,
    TextDecorationLength, TextDecorationLine, TextUnderlinePosition,
};
use style::values::generics::text::{GenericTextDecorationInset, GenericTextDecorationLength};

use crate::color::{Color, ToColorColor as _};
use crate::kurbo_css::{CssBox, Edge, NonUniformRoundedRectRadii};
use crate::render::border::BorderPainter;
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// Outline expansion applied per em when synthesising a bold face, matching the
/// ~0.02 em stem growth browsers use for faux bold.
const FAUX_BOLD_RATIO: f64 = 0.02;

/// One fragment of a non-atomic inline element: the part of it that falls on a
/// single line box. A `<span>` wrapping across three lines has three fragments.
///
/// The rect is the fragment's *content area*, in the inline root's coordinate
/// space: horizontally the union of the fragment's glyph run advances, vertically
/// the union of their fonts' ascent/descent about the baseline (the content area
/// of an inline box is defined by the font, not by the line box).
struct InlineFragment {
    line: usize,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

/// The fragments of one non-atomic inline element, plus its depth below the
/// inline root so that ancestors can be painted before their descendants.
struct InlineDecorations {
    depth: usize,
    fragments: Vec<InlineFragment>,
}

/// The used width of one border edge, in CSS pixels.
///
/// `border-style: none | hidden` forces a used width of zero regardless of the
/// specified `border-width` (CSS backgrounds-3 §4.2), which stylo's computed
/// value does not do for us.
fn border_edge_width(border: &style::properties::style_structs::Border, edge: Edge) -> f64 {
    let (style, width) = match edge {
        Edge::Top => (border.border_top_style, &border.border_top_width),
        Edge::Right => (border.border_right_style, &border.border_right_width),
        Edge::Bottom => (border.border_bottom_style, &border.border_bottom_width),
        Edge::Left => (border.border_left_style, &border.border_left_width),
    };
    match style {
        BorderStyle::None | BorderStyle::Hidden => 0.0,
        _ => width.0.to_f64_px(),
    }
}

/// Draw the backgrounds and borders of the non-atomic inline elements inside an
/// inline formatting context (e.g. `<span style="border: 1px solid">`).
///
/// These elements have no layout box of their own — they are flattened into the
/// inline root's text layout as style spans — so they are never reached by the
/// block-level paint walk and have to be painted from the text layout instead.
/// Each glyph run carries the node id of the innermost inline element it belongs
/// to (via its brush); walking up to the inline root from there gives every
/// element the run is inside, so nested inlines are painted too.
///
/// Per CSS inline-3 §2.2, an inline box that is split across lines draws its
/// block-start/end (top/bottom) borders on every fragment, but its inline-start
/// and inline-end borders only on the first and last fragment respectively — and
/// likewise for padding. The same rule governs where the background box ends.
///
/// Padding and borders on a non-atomic inline box do not affect layout here
/// (they are not fed back into the line breaking), so they are painted around
/// the text rather than displacing it.
///
/// `containing_width` is the width of the inline root's content box in CSS
/// pixels: the containing block width that percentage paddings resolve against.
pub(crate) fn draw_inline_decorations(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: NodeId,
    scale: f64,
    containing_width: f64,
) {
    let mut elements: HashMap<NodeId, InlineDecorations> = HashMap::new();
    // Ancestor path (innermost first) of the item currently being visited.
    let mut chain: Vec<NodeId> = Vec::new();

    for (line_idx, line) in layout.lines().enumerate() {
        for item in line.items() {
            // The node the item belongs to, and the item's content rect. For an
            // atomic inline box the box itself has a layout box (and so paints
            // its own background and border); it contributes to its *ancestors'*
            // fragments, hence starting the walk at its parent.
            let (leaf, x0, x1, y0, y1) = match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => {
                    let metrics = glyph_run.run().font_metrics();
                    let x0 = glyph_run.offset() as f64;
                    let baseline = glyph_run.baseline() as f64;
                    (
                        glyph_run.style().brush.id,
                        x0,
                        x0 + glyph_run.advance() as f64,
                        baseline - metrics.ascent as f64,
                        baseline + metrics.descent as f64,
                    )
                }
                PositionedLayoutItem::InlineBox(inline_box) => {
                    let parent = doc
                        .get_node(NodeId::from_u64(inline_box.id))
                        .and_then(|node| node.parent);
                    let Some(parent) = parent else {
                        continue;
                    };
                    (
                        parent,
                        inline_box.x as f64,
                        (inline_box.x + inline_box.width) as f64,
                        inline_box.y as f64,
                        (inline_box.y + inline_box.height) as f64,
                    )
                }
            };

            // Collect the ancestor path from the leaf up to (but excluding) the
            // inline root. A run belonging directly to the root yields nothing.
            chain.clear();
            let mut id = leaf;
            while id != inline_root_id {
                chain.push(id);
                match doc.get_node(id).and_then(|node| node.parent) {
                    Some(parent) => id = parent,
                    // Detached from the inline root: ignore this item entirely
                    // rather than attributing it to a partial path.
                    None => {
                        chain.clear();
                        break;
                    }
                }
            }

            for (depth, &node_id) in chain.iter().rev().enumerate() {
                let entry = elements
                    .entry(node_id)
                    .or_insert_with(|| InlineDecorations {
                        depth,
                        fragments: Vec::new(),
                    });
                // Items are visited in line order, so an element's fragment for
                // the current line — if it has one — is the one most recently
                // pushed, even when runs of other elements were visited between.
                match entry.fragments.last_mut() {
                    Some(fragment) if fragment.line == line_idx => {
                        fragment.x0 = fragment.x0.min(x0);
                        fragment.x1 = fragment.x1.max(x1);
                        fragment.y0 = fragment.y0.min(y0);
                        fragment.y1 = fragment.y1.max(y1);
                    }
                    _ => entry.fragments.push(InlineFragment {
                        line: line_idx,
                        x0,
                        x1,
                        y0,
                        y1,
                    }),
                }
            }
        }
    }

    // Outermost first, so a nested inline's background paints over its
    // ancestors'. Ties are broken by node id to keep painting deterministic.
    let mut elements: Vec<(NodeId, InlineDecorations)> = elements.into_iter().collect();
    elements.sort_unstable_by_key(|(node_id, decorations)| (decorations.depth, *node_id));

    for (node_id, decorations) in elements {
        let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
            continue;
        };

        let current_color = styles.clone_color();
        let bg_color = styles
            .get_background()
            .background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();

        let border = styles.get_border();
        let border_widths = [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left]
            .map(|edge| border_edge_width(border, edge) * scale);
        let [bw_top, bw_right, bw_bottom, bw_left] = border_widths;

        let has_background = bg_color != Color::TRANSPARENT;
        let has_border = border_widths.iter().any(|w| *w > 0.0);
        if !has_background && !has_border {
            continue;
        }

        let padding = styles.get_padding();
        let resolve_padding = |value: &style::values::computed::LengthPercentage| -> f64 {
            value
                .resolve(CSSPixelLength::new(containing_width as f32))
                .px() as f64
                * scale
        };
        let pad_top = resolve_padding(&padding.padding_top.0);
        let pad_right = resolve_padding(&padding.padding_right.0);
        let pad_bottom = resolve_padding(&padding.padding_bottom.0);
        let pad_left = resolve_padding(&padding.padding_left.0);

        // Which physical edge is the inline-start one. Only horizontal writing
        // modes reach here; vertical ones would map start/end onto top/bottom.
        let rtl = styles.clone_direction() == Direction::Rtl;

        let fragment_count = decorations.fragments.len();
        for (index, fragment) in decorations.fragments.iter().enumerate() {
            let is_first = index == 0;
            let is_last = index + 1 == fragment_count;
            let (has_left, has_right) = match rtl {
                true => (is_last, is_first),
                false => (is_first, is_last),
            };

            let keep = |value: f64, present: bool| if present { value } else { 0.0 };
            let padding_insets = Insets {
                x0: keep(pad_left, has_left),
                y0: pad_top,
                x1: keep(pad_right, has_right),
                y1: pad_bottom,
            };
            let border_insets = Insets {
                x0: keep(bw_left, has_left),
                y0: bw_top,
                x1: keep(bw_right, has_right),
                y1: bw_bottom,
            };

            let content_box = Rect::new(fragment.x0, fragment.y0, fragment.x1, fragment.y1);
            let padding_box = content_box + padding_insets;
            let border_box = padding_box + border_insets;

            // Radii resolve against the fragment's own border box, as they would
            // for a block box of the same size.
            let resolve_w = CSSPixelLength::new((border_box.width() / scale) as f32);
            let resolve_h = CSSPixelLength::new((border_box.height() / scale) as f32);
            let resolve_radii = |radius: &BorderCornerRadius| -> Vec2 {
                Vec2 {
                    x: scale * radius.0.width.0.resolve(resolve_w).px() as f64,
                    y: scale * radius.0.height.0.resolve(resolve_h).px() as f64,
                }
            };
            let border_radii = NonUniformRoundedRectRadii {
                top_left: resolve_radii(&border.border_top_left_radius),
                top_right: resolve_radii(&border.border_top_right_radius),
                bottom_right: resolve_radii(&border.border_bottom_right_radius),
                bottom_left: resolve_radii(&border.border_bottom_left_radius),
            };

            let css_box = CssBox::new(border_box, border_insets, padding_insets, 0.0, border_radii);

            // The background of an inline box covers its padding box.
            if has_background {
                scene.fill(
                    Fill::NonZero,
                    transform,
                    bg_color,
                    None,
                    &css_box.padding_box_path(),
                );
            }

            if has_border {
                BorderPainter {
                    frame: &css_box,
                    style: &styles,
                    transform,
                    scale,
                }
                .draw_border(scene);
            }
        }
    }
}

/// Per-font-face cache of the OS/2 `usWinAscent / unitsPerEm` ratio, keyed by the font
/// blob's unique id and face index. `None` is cached for fonts without a usable OS/2 or
/// head table so they aren't re-parsed either.
type WinAscentCache = HashMap<(u64, u32), Option<f32>>;

/// The font's OS/2 `usWinAscent`, in the same (device) pixels as `font_size`.
///
/// This is the ascent browsers use as the top of the "em box" when positioning overlines. It
/// is typically taller than the hhea ascent Parley exposes via [`parley::layout::run::RunMetrics`].
/// Returns `None` when the font has no OS/2 table.
fn win_ascent(cache: &mut WinAscentCache, font: &parley::FontData, font_size: f32) -> Option<f32> {
    let ratio = *cache
        .entry((font.data.id(), font.index))
        .or_insert_with(|| win_ascent_ratio(font));
    Some(ratio? * font_size)
}

/// The unitless `usWinAscent / unitsPerEm` ratio for a font face, or `None` when the font
/// has no usable head/OS/2 table.
fn win_ascent_ratio(font: &parley::FontData) -> Option<f32> {
    use skrifa::raw::{FontRef, TableProvider as _};
    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let units_per_em = font_ref.head().ok()?.units_per_em();
    let win_ascent = font_ref.os2().ok()?.us_win_ascent();
    Some(win_ascent as f32 / units_per_em as f32)
}

/// Mirrors Blink's `SelectBestDashGap`: choose the gap length (as close as possible to
/// `gap_length`) that fits a whole number of `dash_length` dashes across `stroke_length`,
/// so dashes are evenly distributed and a dash lands at each end of the line.
fn select_best_dash_gap(stroke_length: f64, dash_length: f64, gap_length: f64) -> f64 {
    let available_length = stroke_length + gap_length;
    let min_num_dashes = (available_length / (dash_length + gap_length)).floor();
    let max_num_dashes = min_num_dashes + 1.0;
    let min_num_gaps = min_num_dashes - 1.0;
    let max_num_gaps = max_num_dashes - 1.0;
    if min_num_gaps < 1.0 {
        return gap_length;
    }
    let min_gap = (stroke_length - min_num_dashes * dash_length) / min_num_gaps;
    let max_gap = (stroke_length - max_num_dashes * dash_length) / max_num_gaps;
    if max_gap <= 0.0 || (min_gap - gap_length).abs() < (max_gap - gap_length).abs() {
        min_gap
    } else {
        max_gap
    }
}

/// A text decoration resolved from a single "decorating box" (an element that
/// sets `text-decoration-line`), ready to be drawn over a glyph run. Because the
/// `text-decoration-*` properties do not inherit, decorations are propagated from
/// ancestors rather than inherited, so a run may be covered by several of these.
///
/// Only the run-independent (style-derived) values are stored here so an instance
/// can be cached on the ancestor stack and reused across every run within the same
/// decorating box. The `text-decoration-inset` and `text-underline-offset` values
/// depend on the run's advance/font-size and are resolved lazily when drawing.
#[derive(Clone)]
struct ResolvedDecoration {
    line: TextDecorationLine,
    style: TextDecorationStyle,
    color: Color,
    thickness: TextDecorationLength,
    /// `text-underline-offset`; `None` for `auto`. Resolved per run.
    underline_offset: Option<LengthPercentage>,
    /// Whether `text-underline-position: under` is in effect.
    underline_under: bool,
    /// Whether `text-underline-position: from-font` is in effect.
    underline_from_font: bool,
    /// `text-decoration-inset`. Resolved per run.
    inset: GenericTextDecorationInset<LengthPercentage>,
}

/// An element on the current ancestor path (inline root -> run node), caching the
/// values resolved from its computed style so descending into the same node across
/// consecutive runs doesn't re-resolve them.
struct DecorationStackEntry {
    node_id: NodeId,
    /// This node's (inherited) text colour, used for the glyphs of runs whose
    /// innermost node is this one.
    text_color: Color,
    /// The decoration this node introduces as a decorating box, if any.
    decoration: Option<ResolvedDecoration>,
}

/// Resolve the cached style values for a single node into a [`DecorationStackEntry`].
fn resolve_decoration_entry(doc: &BaseDocument, node_id: NodeId) -> DecorationStackEntry {
    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
        return DecorationStackEntry {
            node_id,
            text_color: Color::BLACK,
            decoration: None,
        };
    };

    let itext = styles.get_inherited_text();
    let text = styles.get_text();
    let text_color = itext.color.as_color_color();

    let drawn_lines = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    let line = text.text_decoration_line;
    // Decorations propagate through the box tree, and a `display: contents` element
    // generates no box, so its decorations have no effect on descendants.
    let is_contents = styles.clone_display().is_contents();
    let decoration = (!is_contents && line.intersects(drawn_lines)).then(|| {
        // `text-decoration-color: currentColor` (the initial value) resolves against
        // the decorating box's own colour, not the descendant run's.
        let color = text
            .text_decoration_color
            .as_absolute()
            .map(ToColorColor::as_color_color)
            .unwrap_or(text_color);

        ResolvedDecoration {
            line,
            style: text.text_decoration_style,
            color,
            thickness: text.text_decoration_thickness.clone(),
            underline_offset: itext.text_underline_offset.non_auto(),
            underline_under: itext
                .text_underline_position
                .contains(TextUnderlinePosition::UNDER),
            underline_from_font: itext
                .text_underline_position
                .contains(TextUnderlinePosition::FROM_FONT),
            inset: text.text_decoration_inset.clone(),
        }
    });

    DecorationStackEntry {
        node_id,
        text_color,
        decoration,
    }
}

/// The run-dependent geometry used to position and size a decoration, captured from a
/// single glyph run. Firefox draws a decoration once for the whole decorating box using
/// the box's own font, so we prefer the values from a run whose innermost node *is* the
/// decorating box (its own text), falling back to the first run it covers.
#[derive(Clone)]
struct DecorationRunGeometry {
    /// The baseline the decoration is positioned from (the run's own, `vertical-align`
    /// shifted, baseline).
    baseline: f32,
    ascent: f32,
    descent: f32,
    underline_offset: f32,
    underline_size: f32,
    strikethrough_size: f32,
    font: parley::FontData,
    font_size: f32,
    css_font_size: f64,
}

/// A decoration accumulated across the runs of a single line for one decorating box.
///
/// The `text-decoration-*` properties describe the whole box, so we gather the horizontal
/// extent it covers on the line (`min_x`/`max_x`) and the font geometry to draw with, then
/// paint a single line spanning the box rather than one segment per run.
struct LineDecoration {
    node_id: NodeId,
    deco: ResolvedDecoration,
    min_x: f64,
    max_x: f64,
    /// Geometry from a run whose innermost node is the decorating box itself.
    own: Option<DecorationRunGeometry>,
    /// Geometry from the first run the box covers (fallback when it has no own text on
    /// this line, e.g. it only wraps differently-sized descendants).
    first: Option<DecorationRunGeometry>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    win_ascent_ratios: WinAscentCache,
}

/// Resolve the CSS `text-decoration-thickness` to a device-pixel size.
///
/// - Percentages are resolved against the font size,
/// - `from-font` uses the metrics from Parley,
/// - `auto` uses a thickness of `font-size / 10` (minimum: 1px).
///
/// The result is floored to a whole number of device pixels (minimum 1), matching
/// browsers, which snap decoration thickness to whole pixels.
fn decoration_size(
    thickness: &TextDecorationLength,
    metric_size: f32,
    css_font_size: f64,
    scale: f64,
) -> f32 {
    (match thickness {
        GenericTextDecorationLength::LengthPercentage(lp) => {
            lp.resolve(Length::new(css_font_size as f32)).px() as f64 * scale
        }
        GenericTextDecorationLength::FromFont => metric_size as f64,
        GenericTextDecorationLength::Auto => (css_font_size / 10.0).max(1.0) * scale,
    })
    .floor()
    .max(1.0) as f32
}

/// Draws a single decoration line of the given `text-decoration-style` spanning
/// `[x0, x0 + width]`. `baseline` is the line's baseline and `offset` positions the line
/// relative to it (`y = baseline - offset + size / 2`, growing downward). Geometry is built
/// explicitly (rather than relying on backend dash support) so it renders consistently
/// across renderers.
///
/// `double_dir` points away from the text (`+1` = down, `-1` = up) and is used to place the
/// second line of a `double` decoration.
#[allow(clippy::too_many_arguments)]
fn draw_decoration_line(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    x0: f64,
    width: f64,
    baseline: f32,
    offset: f32,
    size: f32,
    brush: &anyrender::Paint,
    double_dir: f64,
    deco_style: TextDecorationStyle,
    inset_start: f64,
    inset_end: f64,
) {
    // Inset the line from the box's start/end edges (assumes LTR: start is the left edge).
    // Negative insets extend the line past the text.
    let x = x0 + inset_start;
    let w = width - inset_start - inset_end;
    if w <= 0.0 {
        return;
    }
    let y = (baseline - offset + size / 2.0) as f64;
    let size = size as f64;
    let butt_stroke = Stroke::new(size).with_caps(Cap::Butt);

    match deco_style {
        TextDecorationStyle::MozNone => {
            // no line. Equivalent to `text-decoration-line: none`
        }
        // `solid`
        TextDecorationStyle::Solid => {
            let line = kurbo::Line::new((x, y), (x + w, y));
            scene.stroke(&butt_stroke, transform, brush, None, &line);
        }
        // Two lines, each `size` thick, separated by a 1px (CSS) gap. This
        // matches Chrome, where the second line is offset by `thickness + 1px`
        // and sits further from the text (below for underline, above for
        // overline).
        TextDecorationStyle::Double => {
            let one_css_px = scale;
            for cy in [y, y + double_dir * (size + one_css_px)] {
                let line = kurbo::Line::new((x, cy), (x + w, cy));
                scene.stroke(&butt_stroke, transform, brush, None, &line);
            }
        }
        // Round dots (diameter = line thickness) spaced one dot apart.
        TextDecorationStyle::Dotted => {
            let radius = size / 2.0;
            let step = 2.0 * size;
            let mut cx = x + radius;
            while cx <= x + w - radius {
                let dot = Circle::new((cx, y), radius);
                scene.fill(Fill::NonZero, transform, brush, None, &dot);
                cx += step;
            }
        }
        // Dashes as filled rectangles. Dash/gap lengths are relative to the
        // thickness (matching Blink): thinner lines use proportionally longer
        // dashes and gaps so they don't read as dotted or solid.
        TextDecorationStyle::Dashed => {
            let (dash_ratio, gap_ratio) = if size >= 3.0 { (2.0, 1.0) } else { (3.0, 2.0) };
            let dash = size * dash_ratio;
            let nominal_gap = size * gap_ratio;
            // Nudge the gap so a whole number of dashes fits the run evenly.
            let gap = if w > 2.0 * dash {
                select_best_dash_gap(w, dash, nominal_gap)
            } else {
                nominal_gap
            };
            let mut dx = x;
            while dx < x + w {
                let end = (dx + dash).min(x + w);
                let rect = Rect::new(dx, y - size / 2.0, end, y + size / 2.0);
                scene.fill(Fill::NonZero, transform, brush, None, &rect);
                dx += dash + gap;
            }
        }
        // A squiggle built from alternating quadratic Béziers.
        TextDecorationStyle::Wavy => {
            let amplitude = size;
            let half_wave = (2.0 * size).max(1.0);
            let mut path = BezPath::new();
            path.move_to((x, y));
            let mut px = x;
            let mut up = true;
            while px < x + w {
                let nx = (px + half_wave).min(x + w);
                // A quad Bézier reaches half the control-point offset at its
                // midpoint, so use `2 * amplitude` to hit the target peak.
                let ctrl_y = if up {
                    y - 2.0 * amplitude
                } else {
                    y + 2.0 * amplitude
                };
                path.quad_to(((px + nx) / 2.0, ctrl_y), (nx, y));
                px = nx;
                up = !up;
            }
            let stroke = Stroke::new(size).with_caps(Cap::Round);
            scene.stroke(&stroke, transform, brush, None, &path);
        }
    }
}

/// Paint the decorations accumulated for one line, one line per decorating box.
fn flush_line_decorations(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    deco_boxes: &[LineDecoration],
    win_ascent_ratios: &mut WinAscentCache,
    inline_root_id: NodeId,
    line_baseline: f32,
) {
    // Draw innermost boxes first so ancestors' decorations paint on top, matching the
    // per-run drawing order this replaced (`stack.iter().rev()`).
    for acc in deco_boxes.iter().rev() {
        let deco = &acc.deco;
        // Prefer the decorating box's own font; fall back to the first run it covers.
        let geom = match (&acc.own, &acc.first) {
            (Some(own), _) => own.clone(),
            (None, Some(first)) => {
                let mut geom = first.clone();
                // A descendant run's baseline may be shifted by `vertical-align`, but the
                // decoration is positioned from the decorating box's own baseline. For the
                // inline root that is the line's baseline.
                if acc.node_id == inline_root_id {
                    geom.baseline = line_baseline;
                }
                geom
            }
            (None, None) => continue,
        };
        let geom = &geom;
        let width = acc.max_x - acc.min_x;
        if width <= 0.0 {
            continue;
        }
        let brush = anyrender::Paint::from(deco.color);

        // `text-decoration-inset` shortens (or, when negative, extends) the line from the
        // inline-start/end edges. Percentages resolve against the decoration line length
        // (the box's total advance on this line); `auto` is no inset.
        let (inset_start, inset_end) = match &deco.inset {
            GenericTextDecorationInset::LengthPercentage { start, end } => {
                // The extent is in device pixels; convert to CSS pixels so the resolved
                // result can be scaled back.
                let line_length = Length::new((width / scale) as f32);
                (
                    start.resolve(line_length).px() as f64 * scale,
                    end.resolve(line_length).px() as f64 * scale,
                )
            }
            GenericTextDecorationInset::Auto => (0.0, 0.0),
        };

        if deco.line.contains(TextDecorationLine::UNDERLINE) {
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );

            // `text-underline-offset` moves the underline away from the text. `auto` keeps
            // the font's suggested position; otherwise it resolves against the font size
            // (percentages relative to 1em).
            let extra_underline_offset = deco
                .underline_offset
                .as_ref()
                .map(|lp| lp.resolve(Length::new(geom.css_font_size as f32)).px() as f64 * scale)
                .unwrap_or(0.0);

            // `text-underline-position: under` places the underline below the glyph box
            // (below descenders) rather than at the font's suggested position near the
            // alphabetic baseline. We anchor the top of the line at the descent so it clears
            // descending glyphs like "gqy".
            // An explicit offset under `auto` is taken from the alphabetic
            // baseline, as CSS and Chrome have it, not from the auto gap.
            let base_offset = if deco.underline_under {
                -geom.descent
            } else if deco.underline_offset.is_some() && !deco.underline_from_font {
                0.0
            } else {
                geom.underline_offset
            };
            let offset = base_offset - extra_underline_offset as f32;

            // A `double` underline extends downward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::OVERLINE) {
            // Fonts don't provide a dedicated overline metric, so reuse the underline
            // thickness. The line sits at the top of the "em box": its lower edge rests on
            // the ascent so it clears the glyphs, and it extends upward from there.
            //
            // Browsers use the OS/2 `usWinAscent` for this edge, which is taller than the
            // hhea-based ascent Parley reports; using the smaller value would draw the
            // overline too low. Fall back to Parley's ascent for fonts without a usable
            // OS/2 table.
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent + size;

            // A `double` overline extends upward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                -1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::LINE_THROUGH) {
            let size = decoration_size(
                &deco.thickness,
                geom.strikethrough_size,
                geom.css_font_size,
                scale,
            );

            // Centre the line-through a third of the ascent above the baseline, matching
            // Chrome (which places it `2/3 * ascent` below the text-top). Parley's
            // `strikethrough_offset` (the font's `yStrikeoutPosition`) sits lower and would
            // draw the line too close to the baseline.
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent / 3.0 + size / 2.0;

            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
    }
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
    inline_root_id: NodeId,
    context: &mut DrawTextContext,
) {
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        win_ascent_ratios,
    } = context;
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();

    // Persistent stack mirroring the ancestor path (inline root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    for line in lines {
        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();

        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.font_metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    // `fontique` reports faux-oblique skew in CSS-positive degrees,
                    // while `kurbo::Affine::skew` documents positive X skew in a
                    // Y-up coordinate system. Our screen-space text rendering is
                    // visually Y-down, so invert the sign to preserve CSS italic
                    // direction for synthetic skew.
                    .map(|angle| Affine::skew(-(angle.to_radians().tan() as f64), 0.0));

                let css_font_size = font_size as f64 / scale;

                // Reconcile the stack with this run's ancestor path. Build the path
                // (inline root -> run node), keep the shared prefix already on the stack,
                // pop the rest, and resolve styles only for the newly-descended nodes.
                path_scratch.clear();
                let mut walk_id = Some(style.brush.id);
                while let Some(node_id) = walk_id {
                    path_scratch.push(node_id);
                    if node_id == inline_root_id {
                        break;
                    }
                    walk_id = doc.get_node(node_id).and_then(|node| node.parent);
                }
                path_scratch.reverse();

                let shared = stack
                    .iter()
                    .zip(path_scratch.iter())
                    .take_while(|(entry, node_id)| entry.node_id == **node_id)
                    .count();
                stack.truncate(shared);
                for &node_id in &path_scratch[shared..] {
                    stack.push(resolve_decoration_entry(doc, node_id));
                }

                // The glyph colour comes from the run's own node (the stack top): `color`
                // inherits, so the innermost inline element already carries the right value.
                let text_color = stack.last().map(|e| e.text_color).unwrap_or(Color::BLACK);

                let mut embolden = if FONT_EMBOLDEN_ENABLED {
                    let fs = font_size as f64 / scale;
                    kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3))
                } else {
                    kurbo::Vec2::default()
                };
                // Faux bold: when the resolved family has no heavier face than the
                // requested weight, `fontique` asks for synthetic emboldening. CJK
                // families are usually shipped as a single weight, so without this
                // `<b>`/`font-weight: bold` renders identical to body text.
                if synthesis.embolden() {
                    let fs = font_size as f64;
                    embolden += kurbo::Vec2::new(FAUX_BOLD_RATIO * fs, FAUX_BOLD_RATIO * fs);
                }

                let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
                scene.draw_glyphs(
                    font,
                    font_size,
                    !FONT_EMBOLDEN_ENABLED, // hint
                    normalized_coords,
                    embolden,
                    Fill::NonZero,
                    &anyrender::Paint::from(text_color),
                    1.0, // alpha
                    transform,
                    glyph_xform,
                    glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                        id: glyph.id as _,
                        x: glyph.x,
                        y: glyph.y,
                    }),
                );

                // Accumulate this run's contribution to each decorating box on its ancestor
                // path. The decoration is drawn once per box after the whole line has been
                // walked (see `flush_line_decorations`), so mixed font sizes within a box
                // produce a single straight line rather than one stepped segment per run.
                let geometry = DecorationRunGeometry {
                    // 装饰线跟随上下标一起位移,否则上标的下划线会留在原基线上。
                    baseline: glyph_run.baseline(),
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    underline_offset: metrics.underline_offset,
                    underline_size: metrics.underline_size,
                    strikethrough_size: metrics.strikethrough_size,
                    font: font.clone(),
                    font_size,
                    css_font_size,
                };
                let run_node_id = style.brush.id;
                let run_x0 = glyph_run.offset() as f64;
                let run_x1 = run_x0 + glyph_run.advance() as f64;

                for entry in stack.iter() {
                    if entry.decoration.is_none() {
                        continue;
                    }
                    let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                        Some(idx) => idx,
                        None => {
                            deco_boxes.push(LineDecoration {
                                node_id: entry.node_id,
                                deco: entry.decoration.clone().unwrap(),
                                min_x: f64::INFINITY,
                                max_x: f64::NEG_INFINITY,
                                own: None,
                                first: None,
                            });
                            deco_boxes.len() - 1
                        }
                    };
                    let acc = &mut deco_boxes[idx];
                    acc.min_x = acc.min_x.min(run_x0);
                    acc.max_x = acc.max_x.max(run_x1);
                    if acc.first.is_none() {
                        acc.first = Some(geometry.clone());
                    }
                    // A run whose innermost node is the box itself is the box's own text, so
                    // its font is the one Firefox positions and sizes the decoration with.
                    if entry.node_id == run_node_id {
                        acc.own = Some(geometry.clone());
                    }
                }
            }
        }

        flush_line_decorations(
            scene,
            transform,
            scale,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            line.metrics().baseline,
        );
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
