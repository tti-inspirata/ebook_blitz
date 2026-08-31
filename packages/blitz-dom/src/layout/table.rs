use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::values::computed::Color as ComputedColor;
use style::values::specified::BorderStyle;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout,
};
use taffy::{DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, style_helpers};

use crate::BaseDocument;

use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

#[derive(Clone, Copy)]
enum BorderSide {
    Top,
    Right,
    Bottom,
    Left,
}

/// 取该边的 border-style 与 border-width(CSS px)。
/// border-style 为 none/hidden 时 CSS 规定 border-width 计算值为 0,而 stylo 的
/// `clone_border()` 仍返回 medium(3px),故此处按样式归零。
fn border_side(border: &Border, side: BorderSide) -> (BorderStyle, f32) {
    let (style, width) = match side {
        BorderSide::Top => (border.border_top_style, &border.border_top_width),
        BorderSide::Right => (border.border_right_style, &border.border_right_width),
        BorderSide::Bottom => (border.border_bottom_style, &border.border_bottom_width),
        BorderSide::Left => (border.border_left_style, &border.border_left_width),
    };
    if matches!(style, BorderStyle::None | BorderStyle::Hidden) {
        (style, 0.0)
    } else {
        (style, width.0.to_f32_px())
    }
}

/// 该边的 border-color(未解析 `currentColor`,留到绘制时按元素自身的 color 解析)。
fn border_side_color(border: &Border, side: BorderSide) -> ComputedColor {
    match side {
        BorderSide::Top => border.border_top_color.clone(),
        BorderSide::Right => border.border_right_color.clone(),
        BorderSide::Bottom => border.border_bottom_color.clone(),
        BorderSide::Left => border.border_left_color.clone(),
    }
}

/// 参与边框合并的来源类型。CSS 2.1 §17.6.2 的来源优先级:
/// cell > row > row group > column > column group > table。
///
/// 列与列组不在此列:本实现的表格布局不为 `<col>` / `<colgroup>` 建盒
/// (见 `collect_table_cells` 的 `TableColumn` 分支),它们的边框自然也不参与合并。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum EdgeSource {
    Table = 0,
    RowGroup = 1,
    Row = 2,
    Cell = 3,
}

/// border-style 的合并优先级(CSS 2.1 §17.6.2 第 3 条):
/// double > solid > dashed > dotted > ridge > outset > groove > inset。
fn style_rank(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::Double => 8,
        BorderStyle::Solid => 7,
        BorderStyle::Dashed => 6,
        BorderStyle::Dotted => 5,
        BorderStyle::Ridge => 4,
        BorderStyle::Outset => 3,
        BorderStyle::Groove => 2,
        BorderStyle::Inset => 1,
        BorderStyle::None | BorderStyle::Hidden => 0,
    }
}

/// collapse 模式下一条网格线段合并后的边框。`None` 表示该段不绘制。
#[derive(Debug, Clone, PartialEq)]
pub struct CollapsedEdge {
    pub width: f32,
    pub style: BorderStyle,
    /// 未解析的 border-color;`currentColor` 需按 `color_source` 元素的 color 解析。
    pub color: ComputedColor,
    /// 胜出边框所属的元素,绘制时用来解析 `currentColor`。
    pub color_source: NodeId,
}

/// 一条网格线段的边框合并(CSS 2.1 §17.6.2)。
///
/// 依次比较:`hidden` 一票否决 → 边宽大者胜 → border-style 优先级 → 来源优先级。
/// 全部相同时保留先加入的那个,因此调用方须按「上/左优先」的顺序 `add`。
#[derive(Default)]
struct EdgeResolver {
    hidden: bool,
    best: Option<(CollapsedEdge, EdgeSource)>,
}

impl EdgeResolver {
    fn add(&mut self, border: &Border, side: BorderSide, source: EdgeSource, owner: NodeId) {
        if self.hidden {
            return;
        }
        let (style, width) = border_side(border, side);
        // `hidden` 优先于一切,该段线不绘制。
        if style == BorderStyle::Hidden {
            self.hidden = true;
            self.best = None;
            return;
        }
        if style == BorderStyle::None || width <= 0.0 {
            return;
        }

        let candidate = CollapsedEdge {
            width,
            style,
            color: border_side_color(border, side),
            color_source: owner,
        };
        let wins = match &self.best {
            None => true,
            Some((best, best_source)) => (
                candidate.width,
                style_rank(candidate.style),
                source,
            )
                .gt(&(best.width, style_rank(best.style), *best_source)),
        };
        if wins {
            self.best = Some((candidate, source));
        }
    }

    fn finish(self) -> Option<CollapsedEdge> {
        self.best.map(|(edge, _)| edge)
    }
}

pub struct TableTreeWrapper<'doc> {
    pub(crate) doc: &'doc mut BaseDocument,
    pub(crate) ctx: Arc<TableContext>,
}

#[derive(Debug, Clone)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    pub cells: Vec<TableCell>,
    pub rows: Vec<TableRow>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo<Atom>>>,
    pub border_collapse: BorderCollapse,
    /// 合并后的网格线,仅 `BorderCollapse::Collapse` 下有内容。
    pub collapsed_borders: CollapsedBorders,
}

/// collapse 模式下逐段合并出的网格线。
///
/// 网格有 `n_rows + 1` 条水平线与 `n_cols + 1` 条垂直线,每条线在每个单元格列/行上
/// 是独立的一「段」:段与段的边框可以不同(甚至有的画有的不画),这正是
/// CSS 合并边框模型的要求。被 rowspan/colspan 单元格跨越的段没有线,存 `None`。
#[derive(Debug, Clone, Default)]
pub struct CollapsedBorders {
    pub n_rows: usize,
    pub n_cols: usize,
    /// 水平线段,索引 `line * n_cols + col`,`line ∈ 0..=n_rows`。
    pub horizontal: Vec<Option<CollapsedEdge>>,
    /// 垂直线段,索引 `line * n_rows + row`,`line ∈ 0..=n_cols`。
    pub vertical: Vec<Option<CollapsedEdge>>,
}

impl CollapsedBorders {
    pub fn horizontal(&self, line: usize, col: usize) -> Option<&CollapsedEdge> {
        self.horizontal.get(line * self.n_cols + col)?.as_ref()
    }

    pub fn vertical(&self, line: usize, row: usize) -> Option<&CollapsedEdge> {
        self.vertical.get(line * self.n_rows + row)?.as_ref()
    }

    /// 第 `line` 条水平线上各段的最大边宽。
    pub fn horizontal_width(&self, line: usize) -> f32 {
        (0..self.n_cols)
            .filter_map(|col| self.horizontal(line, col))
            .fold(0.0f32, |acc, edge| acc.max(edge.width))
    }

    /// 第 `line` 条垂直线上各段的最大边宽。
    pub fn vertical_width(&self, line: usize) -> f32 {
        (0..self.n_rows)
            .filter_map(|row| self.vertical(line, row))
            .fold(0.0f32, |acc, edge| acc.max(edge.width))
    }

    /// 第 `line` 条水平线的 gutter 轨道高度。
    fn horizontal_track(&self, line: usize) -> f32 {
        outer_half(self.horizontal_width(line), line, self.n_rows)
    }

    /// 第 `line` 条垂直线的 gutter 轨道宽度。
    fn vertical_track(&self, line: usize) -> f32 {
        outer_half(self.vertical_width(line), line, self.n_cols)
    }
}

/// 网格线占的轨道尺寸。
///
/// CSS 合并模型里每条边线以两个盒子的公共边为中心,一半落在各自盒子内。内部线两侧
/// 都是单元格,整条线都在表格里;最外圈的线只有内侧一半在表格 content-box 内,另一半
/// 溢出到 border-box 之外(表格 `overflow` 非 visible 时会被裁掉,与浏览器一致)。
fn outer_half(width: f32, line: usize, last: usize) -> f32 {
    if line == 0 || line == last {
        width / 2.0
    } else {
        width
    }
}

// #[derive(Debug, Clone, Eq, PartialEq)]
// pub enum TableItemKind {
//     Row,
//     Cell,
// }

#[derive(Debug, Clone)]
pub struct TableCell {
    // kind: TableItemKind,
    pub node_id: NodeId,
    style: taffy::Style<Atom>,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: NodeId,
    pub height: f32,
    /// 该 `<tr>` 自身的边框,参与合并(来源优先级低于单元格)。
    border: ServoArc<Border>,
    /// 所属行组(`<tbody>` / `<thead>` / `<tfoot>`)的节点。
    group: Option<NodeId>,
}

/// 一个行组的边框与它覆盖的行区间(闭区间),用于合并最外圈网格线。
#[derive(Debug, Clone)]
struct RowGroup {
    node_id: NodeId,
    border: ServoArc<Border>,
    first_row: usize,
    last_row: usize,
}

/// 布点前的单元格:带 rowspan/colspan 与自身边框,行列位置由 `place_cells` 填。
pub(crate) struct RawCell {
    node_id: NodeId,
    style: taffy::Style<Atom>,
    border: ServoArc<Border>,
    /// 所在 `<tr>` 的序号,也就是单元格的起始行。
    row: u16,
    col: u16,
    rowspan: u16,
    colspan: u16,
}

impl RawCell {
    /// 网格轨道号(1-indexed 的 taffy grid line)。collapse 下轨道序列是
    /// [线0, 列0, 线1, 列1, …],第 `index` 个单元格轨道是 `2 * index + 2`;
    /// separate 下没有 gutter 轨道,就是 `index + 1`。
    fn grid_line(index: u16, interleaved: bool) -> i16 {
        if interleaved {
            2 * index as i16 + 2
        } else {
            index as i16 + 1
        }
    }

    /// 单元格跨越的轨道数:collapse 下除了自身的 `span` 个单元格轨道,
    /// 还跨过中间的 `span - 1` 条线。
    fn grid_span(span: u16, interleaved: bool) -> u16 {
        if interleaved { 2 * span - 1 } else { span }
    }

    fn into_cell(mut self, interleaved: bool) -> TableCell {
        self.style.grid_column = taffy::Line {
            start: style_helpers::line(Self::grid_line(self.col, interleaved)),
            end: style_helpers::span(Self::grid_span(self.colspan, interleaved)),
        };
        self.style.grid_row = taffy::Line {
            start: style_helpers::line(Self::grid_line(self.row, interleaved)),
            end: style_helpers::span(Self::grid_span(self.rowspan, interleaved)),
        };
        // 宽度由列轨道决定(单元格的 width 已折算进 `column_track`)。
        self.style.size.width = style_helpers::auto();
        TableCell {
            node_id: self.node_id,
            style: self.style,
        }
    }
}

/// 布点结果:每个网格位置上的单元格下标(`None` = 空位)。
struct CellGrid {
    n_rows: usize,
    n_cols: usize,
    slots: Vec<Option<usize>>,
}

impl CellGrid {
    fn at(&self, row: usize, col: usize) -> Option<usize> {
        self.slots.get(row * self.n_cols + col).copied().flatten()
    }
}

/// HTML 表格布点:每行从左往右扫,跳过被上方 rowspan 占住的格子。
///
/// 早先是靠 taffy 的 `GridAutoFlow::RowDense` 自动放置来近似这套规则,但合并边框需要
/// 知道**每个格子上坐着谁**(相邻两格的边框才能合并),所以这里显式算一遍。
fn place_cells(cells: &mut [RawCell], n_rows: usize) -> CellGrid {
    // 每列的占用情况按行展开,列数未知时先用一张够宽的稀疏表(按需扩列)。
    let mut occupied: Vec<Vec<Option<usize>>> = vec![Vec::new(); n_rows];
    let mut n_cols = 0usize;

    for (index, cell) in cells.iter_mut().enumerate() {
        let row = cell.row as usize;
        let (rowspan, colspan) = (cell.rowspan as usize, cell.colspan as usize);

        // 该行第一个未被占用的列。
        let mut col = 0usize;
        while occupied[row].get(col).copied().flatten().is_some() {
            col += 1;
        }
        cell.col = col as u16;

        let end_col = col + colspan;
        n_cols = n_cols.max(end_col);
        for row_slots in &mut occupied[row..(row + rowspan).min(n_rows)] {
            if row_slots.len() < end_col {
                row_slots.resize(end_col, None);
            }
            for slot in &mut row_slots[col..end_col] {
                *slot = Some(index);
            }
        }
    }

    let mut slots = vec![None; n_rows * n_cols];
    for (row, row_slots) in occupied.iter().enumerate() {
        for (col, slot) in row_slots.iter().enumerate() {
            slots[row * n_cols + col] = *slot;
        }
    }

    CellGrid {
        n_rows,
        n_cols,
        slots,
    }
}

/// 把单元格轨道与 gutter 轨道交错成 `[线0, 轨道0, 线1, 轨道1, …, 线n]`。
fn interleave_tracks(
    tracks: Vec<taffy::TrackSizingFunction>,
    count: usize,
    line_width: impl Fn(usize) -> f32,
) -> Vec<taffy::GridTemplateComponent<Atom>> {
    let mut out = Vec::with_capacity(2 * count + 1);
    for (index, track) in tracks.into_iter().enumerate() {
        let line: taffy::TrackSizingFunction = style_helpers::length(line_width(index));
        out.push(line.into());
        out.push(track.into());
    }
    let last: taffy::TrackSizingFunction = style_helpers::length(line_width(count));
    out.push(last.into());
    out
}

/// 单元格的 `width` 转成列轨道的尺寸函数。
fn column_track(style: &taffy::Style<Atom>, is_fixed: bool) -> taffy::TrackSizingFunction {
    match style.size.width.tag() {
        taffy::CompactLength::LENGTH_TAG => {
            let len = style.size.width.value();
            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
            let border = style.border.resolve_or_zero(None, resolve_calc_value);
            match style.box_sizing {
                taffy::BoxSizing::ContentBox => style_helpers::length(
                    len + padding.left + padding.right + border.left + border.right,
                ),
                taffy::BoxSizing::BorderBox => style_helpers::length(len),
            }
        }
        taffy::CompactLength::PERCENT_TAG => {
            if is_fixed {
                style_helpers::percent(style.size.width.value())
            } else {
                style_helpers::auto()
            }
        }
        taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
        // Dimension values are always length, percentage, auto or calc(),
        // so any other tag is a calc() value. Pass it through so that
        // Taffy resolves it against the table's inner width.
        _ => style.size.width.into(),
    }
}

/// 逐段合并整张网格的边框(CSS 2.1 §17.6.2)。
///
/// 每条线段的参与者是它两侧的单元格,加上该段落在行/行组/表格边界上时的那些边框。
/// 被 rowspan/colspan 单元格跨越的段没有线。
fn resolve_collapsed_borders(
    doc: &BaseDocument,
    grid: &CellGrid,
    cells: &[RawCell],
    rows: &[TableRow],
    table_border: &ServoArc<Border>,
    table_node_id: NodeId,
) -> CollapsedBorders {
    let (n_rows, n_cols) = (grid.n_rows, grid.n_cols);
    let groups = collect_row_groups(doc, rows);

    let mut horizontal = vec![None; (n_rows + 1) * n_cols];
    let mut vertical = vec![None; (n_cols + 1) * n_rows];

    // 水平线:线 `line` 在列 `col` 上的一段。
    for line in 0..=n_rows {
        for col in 0..n_cols {
            let above = (line > 0).then(|| grid.at(line - 1, col)).flatten();
            let below = (line < n_rows).then(|| grid.at(line, col)).flatten();
            // 同一个单元格跨在线的两侧 —— 线穿过单元格内部,不画。
            if above.is_some() && above == below {
                continue;
            }

            let mut resolver = EdgeResolver::default();
            // 顺序即同级时的取胜顺序:上/左优先(CSS 2.1 §17.6.2 第 4 条)。
            if let Some(index) = above {
                let cell = &cells[index];
                resolver.add(&cell.border, BorderSide::Bottom, EdgeSource::Cell, cell.node_id);
            }
            if let Some(index) = below {
                let cell = &cells[index];
                resolver.add(&cell.border, BorderSide::Top, EdgeSource::Cell, cell.node_id);
            }
            if let Some(row) = line.checked_sub(1).and_then(|r| rows.get(r)) {
                resolver.add(&row.border, BorderSide::Bottom, EdgeSource::Row, row.node_id);
            }
            if let Some(row) = rows.get(line) {
                resolver.add(&row.border, BorderSide::Top, EdgeSource::Row, row.node_id);
            }
            for group in &groups {
                if line > 0 && group.last_row == line - 1 {
                    resolver.add(
                        &group.border,
                        BorderSide::Bottom,
                        EdgeSource::RowGroup,
                        group.node_id,
                    );
                }
                if group.first_row == line {
                    resolver.add(
                        &group.border,
                        BorderSide::Top,
                        EdgeSource::RowGroup,
                        group.node_id,
                    );
                }
            }
            if line == 0 {
                resolver.add(table_border, BorderSide::Top, EdgeSource::Table, table_node_id);
            }
            if line == n_rows {
                resolver.add(
                    table_border,
                    BorderSide::Bottom,
                    EdgeSource::Table,
                    table_node_id,
                );
            }

            horizontal[line * n_cols + col] = resolver.finish();
        }
    }

    // 垂直线:线 `line` 在行 `row` 上的一段。行与行组横跨整张表,它们的左右边框只在
    // 最外侧的两条竖线上参与合并。
    for line in 0..=n_cols {
        let outer_side = match line {
            0 => Some(BorderSide::Left),
            _ if line == n_cols => Some(BorderSide::Right),
            _ => None,
        };
        for row in 0..n_rows {
            let left = (line > 0).then(|| grid.at(row, line - 1)).flatten();
            let right = (line < n_cols).then(|| grid.at(row, line)).flatten();
            if left.is_some() && left == right {
                continue;
            }

            let mut resolver = EdgeResolver::default();
            if let Some(index) = left {
                let cell = &cells[index];
                resolver.add(&cell.border, BorderSide::Right, EdgeSource::Cell, cell.node_id);
            }
            if let Some(index) = right {
                let cell = &cells[index];
                resolver.add(&cell.border, BorderSide::Left, EdgeSource::Cell, cell.node_id);
            }
            if let Some(side) = outer_side {
                if let Some(table_row) = rows.get(row) {
                    resolver.add(&table_row.border, side, EdgeSource::Row, table_row.node_id);
                }
                for group in &groups {
                    if (group.first_row..=group.last_row).contains(&row) {
                        resolver.add(&group.border, side, EdgeSource::RowGroup, group.node_id);
                    }
                }
                resolver.add(table_border, side, EdgeSource::Table, table_node_id);
            }

            vertical[line * n_rows + row] = resolver.finish();
        }
    }

    CollapsedBorders {
        n_rows,
        n_cols,
        horizontal,
        vertical,
    }
}

/// 把各行携带的行组下标折算成「组 → 行区间」。
fn collect_row_groups(doc: &BaseDocument, rows: &[TableRow]) -> Vec<RowGroup> {
    let mut groups: Vec<RowGroup> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let Some(group_id) = row.group else { continue };
        match groups.iter_mut().find(|g| g.node_id == group_id) {
            Some(group) => group.last_row = index,
            None => {
                let Some(border) = doc
                    .nodes
                    .get(group_id)
                    .and_then(|node| node.primary_styles())
                    .map(|styles| styles.clone_border())
                else {
                    continue;
                };
                groups.push(RowGroup {
                    node_id: group_id,
                    border,
                    first_row: index,
                    last_row: index,
                });
            }
        }
    }
    groups
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: NodeId,
) -> (TableContext, Vec<NodeId>) {
    let mut rows: Vec<TableRow> = Vec::new();
    let mut row = 0u16;

    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.item_is_table = true;
    // 单元格的行列位置由 `place_cells` 显式算出(HTML 表格布点),不用 taffy 的自动放置。
    style.grid_auto_flow = taffy::GridAutoFlow::Row;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;
    let table_border = stylo_styles.clone_border();

    drop(stylo_styles);

    let mut raw_cells: Vec<RawCell> = Vec::new();
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
            border_collapse,
            None,
            &mut row,
            &mut raw_cells,
            &mut rows,
        );
    }

    // 显式布点:得到每个单元格的行列位置与整张网格的占用情况。
    let grid = place_cells(&mut raw_cells, rows.len());
    let (n_rows, n_cols) = (grid.n_rows, grid.n_cols);

    // 第一行的单元格提供各列的列宽(与显式布点前的行为一致)。
    let mut column_sizes: Vec<taffy::TrackSizingFunction> =
        vec![style_helpers::auto(); n_cols];
    for cell in raw_cells.iter().filter(|cell| cell.row == 0) {
        if cell.colspan == 1 && (cell.col as usize) < n_cols {
            column_sizes[cell.col as usize] = column_track(&cell.style, is_fixed);
        }
    }

    let collapsed_borders = match border_collapse {
        BorderCollapse::Collapse => {
            resolve_collapsed_borders(doc, &grid, &raw_cells, &rows, &table_border, table_root_node_id)
        }
        BorderCollapse::Separate => CollapsedBorders::default(),
    };

    // collapse 模式下每条网格线占一条独立的 gutter 轨道,轨道宽 = 该线各段的最大边宽。
    // 轨道序列因此是 [线0, 列0, 线1, 列1, …, 列n-1, 线n],单元格落在奇数号轨道上
    // (`RawCell::grid_line`)。
    //
    // separate 模式保持每列一条轨道:那里的间距由 `gap` 提供,而 gap 作用于**每对**
    // 相邻轨道,插了 gutter 就会把间距翻倍。
    let is_collapse = border_collapse == BorderCollapse::Collapse;
    let row_sizes = vec![style_helpers::auto(); n_rows];
    if is_collapse {
        style.grid_template_columns =
            interleave_tracks(column_sizes, n_cols, |line| collapsed_borders.vertical_track(line));
        style.grid_template_rows =
            interleave_tracks(row_sizes, n_rows, |line| collapsed_borders.horizontal_track(line));
    } else {
        style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
        style.grid_template_rows = row_sizes.into_iter().map(|dim| dim.into()).collect();
    }

    style.gap = match border_collapse {
        BorderCollapse::Separate => {
            // In the separated borders model, `border-spacing` also applies between
            // the table border and the outermost cells, in addition to between cells.
            let spacing_x = border_spacing.width.px();
            let spacing_y = border_spacing.height.px();
            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
            style.padding = taffy::Rect {
                left: style_helpers::length(padding.left + spacing_x),
                right: style_helpers::length(padding.right + spacing_x),
                top: style_helpers::length(padding.top + spacing_y),
                bottom: style_helpers::length(padding.bottom + spacing_y),
            };
            taffy::Size {
                width: style_helpers::length(spacing_x),
                height: style_helpers::length(spacing_y),
            }
        }
        // 间距全部由 gutter 轨道承担,gap 会额外叠加在每对轨道之间,必须归零。
        BorderCollapse::Collapse => taffy::Size::ZERO.map(style_helpers::length),
    };

    if is_collapse {
        // 合并模型下表格自身的 border 已经并入最外圈网格线(gutter 轨道 0 与 2·n),
        // 不再单独占位,否则边线会被推进 content-box 而与表格 border 叠加。
        style.border = taffy::Rect::ZERO.map(style_helpers::length);
    }

    let cells: Vec<TableCell> = raw_cells
        .into_iter()
        .map(|cell| cell.into_cell(is_collapse))
        .collect();

    let layout_children = cells.iter().map(|cell| cell.node_id).collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            cells,
            rows,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            collapsed_borders,
        },
        layout_children,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: NodeId,
    border_collapse: BorderCollapse,
    group: Option<NodeId>,
    row: &mut u16,
    cells: &mut Vec<RawCell>,
    rows: &mut Vec<TableRow>,
) {
    let node = &mut doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
        #[cfg(feature = "tracing")]
        tracing::info!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            // `display: contents` 不生成盒,不构成行组;真正的行组才往下传。
            let is_row_group = !matches!(display.inside(), DisplayInside::Contents);
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(
                    doc,
                    child_id,
                    border_collapse,
                    is_row_group.then_some(node_id).or(group),
                    row,
                    cells,
                    rows,
                );
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableRow => {
            let border = node.primary_styles().unwrap().clone_border();
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);

            rows.push(TableRow {
                node_id,
                height: 0.0,
                border,
                group,
            });
            *row = (rows.len() - 1) as u16;

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(doc, child_id, border_collapse, group, row, cells, rows);
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse().ok())
                .unwrap_or(1);
            let rowspan: u16 = node
                .attr(local_name!("rowspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.clamp(1, 65534))
                .unwrap_or(1);
            let mut style = stylo_taffy::to_taffy_style(stylo_style);
            let border = stylo_style.clone_border();

            // Zero-out cell borders is BorderCollapse is Collapse
            // Borders are handled at the table level in this mode
            if border_collapse == BorderCollapse::Collapse {
                style.border = taffy::Rect::ZERO.map(style_helpers::length);
            }

            // The margin properties do not apply to table-internal elements
            style.margin = taffy::Rect::ZERO.map(style_helpers::length);

            // 行列位置留给 `place_cells` 显式布点(`RawCell::into_cell` 写进 style)。
            cells.push(RawCell {
                node_id,
                style,
                border,
                row: *row,
                col: 0,
                rowspan,
                colspan: colspan.max(1),
            });
        }
        DisplayInside::Flow
        | DisplayInside::FlowRoot
        | DisplayInside::Flex
        | DisplayInside::Grid => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Probably a table caption: ignore
            // println!(
            //     "Warning: ignoring non-table typed descendent of table ({:?})",
            //     display.inside()
            // );
        }
        DisplayInside::TableColumnGroup | DisplayInside::TableColumn | DisplayInside::Table => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            //Ignore
        }
        DisplayInside::None => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Ignore
        }
    }
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = crate::taffy_node_id(self.ctx.cells[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.cells[usize::from(node_id)];
        let node_id = crate::taffy_node_id(cell.node_id);
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }

    fn set_detailed_grid_info(
        &mut self,
        _node_id: taffy::NodeId,
        detailed_grid_info: DetailedGridInfo<Atom>,
    ) {
        *self.ctx.computed_grid_info.borrow_mut() = Some(detailed_grid_info);
    }
}
