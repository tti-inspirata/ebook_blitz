use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::typed_om::NumericBaseType;
use style::values::computed::calc::CalcPercentageLeaf;
use style::values::computed::length_percentage::{
    CalcLengthPercentage, CalcNode, ComputedLeaf, Unpacked as UnpackedLengthPercentage,
};
use style::values::computed::Color as ComputedColor;
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::Optional;
use style::values::specified::BorderStyle;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout,
};
use style_traits::values::specified::AllowedNumericType;
use taffy::{
    DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, style_helpers,
};

use crate::BaseDocument;
use crate::layout::LayoutPassState;

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
/// 列与列组暂不参与:`collect_columns` 只取 `<col>` / `<colgroup>` 的宽度与背景,
/// 不取它们的边框。
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

pub struct TableTreeWrapper<'a, 'doc> {
    pub(crate) doc: &'a mut LayoutPassState<'doc>,
    pub(crate) ctx: Arc<TableContext>,
}

// Deliberately not `Clone`: `style` may hold raw pointers into `calc_values`.
#[derive(Debug)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    pub cells: Vec<TableCell>,
    /// Absolutely positioned descendants of the table's row groups and rows (and of the
    /// table itself). They take no part in the table grid.
    pub oof_children: Vec<(NodeId, taffy::Position)>,
    pub rows: Vec<TableRow>,
    pub columns: Vec<TableColumn>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo<Atom>>>,
    pub border_collapse: BorderCollapse,
    /// 合并后的网格线,仅 `BorderCollapse::Collapse` 下有内容。
    pub collapsed_borders: CollapsedBorders,
    /// Backing storage for `calc()` track sizes synthesised by the table layout code.
    /// Taffy stores calc values as raw pointers, so these must outlive the `style`.
    #[allow(dead_code)]
    calc_values: Vec<LengthPercentage>,
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
    pub fn horizontal_track(&self, line: usize) -> f32 {
        outer_half(self.horizontal_width(line), line, self.n_rows)
    }

    /// 第 `line` 条垂直线的 gutter 轨道宽度。
    pub fn vertical_track(&self, line: usize) -> f32 {
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
    /// Index (into `TableContext::rows`) of the row the cell originates in
    pub row: u16,
    /// Index of the column the cell originates in
    pub column: u16,
    pub rowspan: u16,
    pub colspan: u16,
    style: taffy::Style<Atom>,
    /// 单元格自身的边框,参与 collapse 合并。
    border: ServoArc<Border>,
}

/// Tracks the current column position while walking the table's cells, so that
/// cells are assigned the same columns Taffy's dense auto-placement will give them
/// (skipping columns still occupied by rowspan cells from earlier rows).
#[derive(Debug, Default)]
struct ColumnCursor {
    /// The column the next cell in the current row will be placed in
    col: u16,
    /// The total number of columns seen so far
    num_columns: u16,
    /// For each column, the number of rows (counting the current one) it is occupied by a
    /// rowspan cell. `start_row` counts the new row off before cells are placed in it.
    rowspans: Vec<u16>,
}

impl ColumnCursor {
    fn start_row(&mut self) {
        self.col = 0;
        for remaining in self.rowspans.iter_mut() {
            *remaining = remaining.saturating_sub(1);
        }
    }

    /// Advance past occupied columns, returning the column of the next cell
    fn next_free(&mut self) -> u16 {
        while self.rowspans.get(self.col as usize).is_some_and(|r| *r > 0) {
            self.col += 1;
        }
        self.col
    }

    fn place(&mut self, colspan: u16, rowspan: u16) {
        let end = self.col + colspan;
        if self.rowspans.len() < end as usize {
            self.rowspans.resize(end as usize, 0);
        }
        // `rowspan` rather than `rowspan - 1`: `start_row` decrements before the next row is
        // scanned, so a cell spanning two rows must still read as occupied then.
        for remaining in &mut self.rowspans[self.col as usize..end as usize] {
            *remaining = rowspan;
        }
        self.col = end;
        self.num_columns = self.num_columns.max(end);
    }
}

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub node_id: NodeId,
    /// The column group (if any) that contains the column
    pub group: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: NodeId,
    /// The row group (if any) that contains the row
    pub group: Option<NodeId>,
    /// The row's specified `height`, if it is a length
    pub height: Option<f32>,
    /// Whether the row or any cell originating in it (and spanning only this row) has a
    /// specified length `height`. Such rows do not receive excess table height when there
    /// are unconstrained rows to receive it instead.
    pub is_constrained: bool,
    /// 该 `<tr>` 自身的边框,参与合并(来源优先级低于单元格)。
    border: ServoArc<Border>,
}

/// 一个行组的边框与它覆盖的行区间(闭区间),用于合并最外圈网格线。
#[derive(Debug, Clone)]
struct RowGroup {
    node_id: NodeId,
    border: ServoArc<Border>,
    first_row: usize,
    last_row: usize,
}

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

/// 布点结果:每个网格位置上的单元格下标(`None` = 空位)。
struct CellGrid {
    n_rows: usize,
    n_cols: usize,
    slots: Vec<Option<usize>>,
}

impl CellGrid {
    /// 由 `ColumnCursor` 算好的各单元格行列位置展开成占位表。
    ///
    /// 早先是靠 taffy 的 `GridAutoFlow::RowDense` 自动放置来定列,但合并边框需要
    /// 知道**每个格子上坐着谁**(相邻两格的边框才能合并),所以单元格一律显式布点。
    fn new(cells: &[TableCell], n_rows: usize, n_cols: usize) -> Self {
        let mut slots = vec![None; n_rows * n_cols];
        for (index, cell) in cells.iter().enumerate() {
            let (row, col) = (cell.row as usize, cell.column as usize);
            let row_end = (row + cell.rowspan as usize).min(n_rows);
            let col_end = (col + cell.colspan as usize).min(n_cols);
            for r in row..row_end {
                for slot in &mut slots[r * n_cols + col..r * n_cols + col_end] {
                    *slot = Some(index);
                }
            }
        }
        CellGrid {
            n_rows,
            n_cols,
            slots,
        }
    }

    fn at(&self, row: usize, col: usize) -> Option<usize> {
        self.slots.get(row * self.n_cols + col).copied().flatten()
    }
}

/// 把单元格轨道与 gutter 轨道交错成 `[线0, 轨道0, 线1, 轨道1, …, 线n]`。
fn interleave_tracks(
    tracks: Vec<TrackSizingFunction>,
    line_width: impl Fn(usize) -> f32,
) -> Vec<taffy::GridTemplateComponent<Atom>> {
    let count = tracks.len();
    let mut out = Vec::with_capacity(2 * count + 1);
    for (index, track) in tracks.into_iter().enumerate() {
        let line: TrackSizingFunction = style_helpers::length(line_width(index));
        out.push(line.into());
        out.push(track.into());
    }
    let last: TrackSizingFunction = style_helpers::length(line_width(count));
    out.push(last.into());
    out
}

/// 逐段合并整张网格的边框(CSS 2.1 §17.6.2)。
///
/// 每条线段的参与者是它两侧的单元格,加上该段落在行/行组/表格边界上时的那些边框。
/// 被 rowspan/colspan 单元格跨越的段没有线。
fn resolve_collapsed_borders(
    doc: &BaseDocument,
    grid: &CellGrid,
    cells: &[TableCell],
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

/// 把各行携带的行组折算成「组 → 行区间」。
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

/// The writing mode the table's grid runs in (the table's own)
#[cfg_attr(not(feature = "writing-mode"), allow(unused_variables))]
fn table_wm(doc: &BaseDocument, table_root_node_id: NodeId) -> WritingMode {
    #[cfg(feature = "writing-mode")]
    {
        if doc.try_root_element().map(|root| root.id) == Some(table_root_node_id) {
            doc.root_layout_wm(table_root_node_id)
        } else {
            doc.nodes[table_root_node_id].writing_mode()
        }
    }
    #[cfg(not(feature = "writing-mode"))]
    {
        WritingMode::empty()
    }
}

/// Convert a table part's style into the table's writing mode
fn table_taffy_style(style: &ComputedValues, table_wm: WritingMode) -> taffy::Style<Atom> {
    stylo_taffy::to_taffy_style_in(style, table_wm)
}

/// Build a `calc(<percent> + <length>)` track sizing function. The calc value is
/// boxed and stored in `calc_values` so that the raw pointer Taffy holds stays valid.
fn percent_plus_length(
    calc_values: &mut Vec<LengthPercentage>,
    percent: f32,
    length: f32,
) -> TrackSizingFunction {
    let node = CalcNode::Sum(
        vec![
            CalcNode::Leaf(ComputedLeaf::Percentage(CalcPercentageLeaf::new(
                percent,
                Optional::Some(NumericBaseType::Length),
            ))),
            CalcNode::Leaf(ComputedLeaf::Length(Length::new(length))),
        ]
        .into(),
    );
    let value = LengthPercentage::new_calc(node, AllowedNumericType::NonNegative);
    let dim = match value.unpack() {
        UnpackedLengthPercentage::Calc(calc) => {
            let ptr = calc as *const CalcLengthPercentage as *const ();
            // SAFETY: the pointer targets the heap allocation owned by `value`, which
            // is kept alive by `calc_values` for as long as the TableContext exists.
            unsafe { taffy::Dimension::from_raw(taffy::CompactLength::calc(ptr)) }
        }
        UnpackedLengthPercentage::Length(len) => style_helpers::length(len.px()),
        UnpackedLengthPercentage::Percentage(p) => style_helpers::percent(p.0),
    };
    calc_values.push(value);
    dim.into()
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: NodeId,
) -> (TableContext, Vec<NodeId>) {
    let mut cells: Vec<TableCell> = Vec::new();
    let mut oof_children: Vec<(NodeId, taffy::Position)> = Vec::new();
    let mut rows: Vec<TableRow> = Vec::new();
    let mut row = 0u16;
    let mut cursor = ColumnCursor::default();

    let wm = table_wm(doc, table_root_node_id);
    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = table_taffy_style(&stylo_styles, wm);
    style.item_is_table = true;
    // 单元格的行列位置由 `ColumnCursor` 显式算出(HTML 表格布点,见 `CellGrid::new`),
    // 不用 taffy 的自动放置。
    style.grid_auto_flow = taffy::GridAutoFlow::Row;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    // The fixed table layout algorithm only applies when the table has a non-auto width
    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => !style.size.width.is_auto(),
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;
    let table_border = stylo_styles.clone_border();

    drop(stylo_styles);

    let mut columns: Vec<TableColumn> = Vec::new();
    let mut column_sizes: Vec<taffy::TrackSizingFunction> = Vec::new();
    for child_id in children.iter().copied() {
        collect_columns(doc, child_id, wm, &mut columns, &mut column_sizes);
    }
    // Percentage column widths only take effect in the fixed table layout algorithm
    if !is_fixed {
        for column in column_sizes.iter_mut() {
            if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = style_helpers::auto();
            }
        }
    }
    // Percentage widths set on first-row cells: (column index, percentage, padding + border)
    let mut percent_columns: Vec<(u16, f32, f32)> = Vec::new();
    let mut calc_values: Vec<LengthPercentage> = Vec::new();
    // Header row groups are laid out before other rows, and footer row groups after
    let row_group_order = |doc: &BaseDocument, child_id: NodeId| -> u8 {
        let display = doc.nodes[child_id]
            .primary_styles()
            .map(|s| s.clone_display());
        match display.map(|d| d.inside()) {
            Some(DisplayInside::TableHeaderGroup) => 0,
            Some(DisplayInside::TableFooterGroup) => 2,
            _ => 1,
        }
    };
    for order in 0..3 {
        for child_id in children.iter().copied() {
            if row_group_order(doc, child_id) != order {
                continue;
            }
            collect_table_cells(
                doc,
                child_id,
                wm,
                is_fixed,
                border_collapse,
                &mut row,
                &mut cursor,
                &mut cells,
                &mut oof_children,
                &mut rows,
                &mut column_sizes,
                &mut percent_columns,
            );
        }
    }
    let remaining_column = if is_fixed {
        style_helpers::minmax(style_helpers::length(0.0), style_helpers::fr(1.0))
    } else {
        style_helpers::auto()
    };
    // Trailing `<col>` columns which contain no cells and have no definite width
    // (i.e. would be zero-width) are dropped, so they don't add border-spacing.
    while column_sizes.len() > cursor.num_columns as usize
        && column_sizes
            .last()
            .is_some_and(|c| c.min.into_raw().tag() == taffy::CompactLength::AUTO_TAG)
    {
        column_sizes.pop();
        columns.pop();
    }
    let num_columns = cursor.num_columns.max(column_sizes.len() as u16);
    column_sizes.resize(num_columns as usize, remaining_column);
    if is_fixed {
        // In the fixed table layout algorithm, percentage column widths resolve against
        // the table's content width minus the horizontal border-spacing between columns,
        // whereas Taffy resolves percentage tracks against the container's inner width
        // (from which only the outer border-spacing has been removed via padding). Fold
        // the inner border-spacing into the percentage using `calc(N% - N * spacing)`.
        let spacing_x = match border_collapse {
            BorderCollapse::Separate => border_spacing.width.px(),
            BorderCollapse::Collapse => 0.0,
        };
        let inner_spacing = spacing_x * num_columns.saturating_sub(1) as f32;
        let mut percent_track = |percent: f32, extra: f32| -> TrackSizingFunction {
            let extra = extra - percent * inner_spacing;
            if extra == 0.0 {
                style_helpers::percent(percent)
            } else {
                percent_plus_length(&mut calc_values, percent, extra)
            }
        };

        for column in column_sizes.iter_mut() {
            if column.max.is_auto() {
                *column = remaining_column;
            } else if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = percent_track(column.max.into_raw().value(), 0.0);
            }
        }

        // Percentage widths on cells apply to the cell's content box, so the
        // cell's horizontal padding and border must be added to the column width.
        for (col_idx, percent, extra) in percent_columns {
            if let Some(column) = column_sizes.get_mut(col_idx as usize) {
                *column = percent_track(percent, extra);
            }
        }
    }

    // A row's `height` is a minimum: the row grows to fit the content of its cells.
    // Excess table height is distributed to unconstrained rows (via the stretching of
    // `auto` tracks) if there are any, and to all rows otherwise. So constrained rows
    // use a `max-content` (rather than `auto`) maximum when there are unconstrained rows.
    let has_unconstrained_rows = rows.iter().any(|row| !row.is_constrained);
    let row_sizes: Vec<TrackSizingFunction> = rows
        .iter()
        .map(|row| {
            let min = match row.height {
                Some(height) => style_helpers::length(height),
                None => style_helpers::auto(),
            };
            let max = if row.is_constrained && has_unconstrained_rows {
                style_helpers::max_content()
            } else {
                style_helpers::auto()
            };
            style_helpers::minmax(min, max)
        })
        .collect();

    // HTML 表格模型:rowspan 截到所在行组的末行(直接挂在表格下的连续 `<tr>` 视作一个
    // 隐式行组)。不截的话,越界的那几行会让 taffy 生成隐式轨道,collapse 的交错轨道
    // 序列随之错位,整张表的线都画不出来。
    let mut group_end = vec![0usize; rows.len()];
    let mut start = 0;
    while start < rows.len() {
        let group = rows[start].group;
        let mut end = start;
        while end + 1 < rows.len() && rows[end + 1].group == group {
            end += 1;
        }
        group_end[start..=end].fill(end);
        start = end + 1;
    }
    for cell in &mut cells {
        if let Some(&end) = group_end.get(cell.row as usize) {
            let max_span = (end - cell.row as usize + 1) as u16;
            cell.rowspan = cell.rowspan.min(max_span);
        }
    }

    let is_collapse = border_collapse == BorderCollapse::Collapse;
    let collapsed_borders = if is_collapse {
        let grid = CellGrid::new(&cells, rows.len(), num_columns as usize);
        resolve_collapsed_borders(doc, &grid, &cells, &rows, &table_border, table_root_node_id)
    } else {
        CollapsedBorders::default()
    };

    // collapse 模式下每条网格线占一条独立的 gutter 轨道,轨道宽 = 该线各段的最大边宽。
    // 轨道序列因此是 [线0, 列0, 线1, 列1, …, 列n-1, 线n],单元格落在偶数号 grid line
    // 起始的轨道上(`grid_line`)。
    //
    // separate 模式保持每列一条轨道:那里的间距由 `gap` 提供,而 gap 作用于**每对**
    // 相邻轨道,插了 gutter 就会把间距翻倍。
    if is_collapse {
        style.grid_template_columns =
            interleave_tracks(column_sizes, |line| collapsed_borders.vertical_track(line));
        style.grid_template_rows =
            interleave_tracks(row_sizes, |line| collapsed_borders.horizontal_track(line));
    } else {
        style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
        style.grid_template_rows = row_sizes.into_iter().map(|dim| dim.into()).collect();
    }
    for cell in &mut cells {
        cell.style.grid_column = taffy::Line {
            start: style_helpers::line(grid_line(cell.column, is_collapse)),
            end: style_helpers::span(grid_span(cell.colspan, is_collapse)),
        };
        cell.style.grid_row = taffy::Line {
            start: style_helpers::line(grid_line(cell.row, is_collapse)),
            end: style_helpers::span(grid_span(cell.rowspan, is_collapse)),
        };
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

    let layout_children = cells
        .iter()
        .map(|cell| cell.node_id)
        .chain(oof_children.iter().map(|(node_id, _)| *node_id))
        .collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            cells,
            oof_children,
            rows,
            columns,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            collapsed_borders,
            calc_values,
        },
        layout_children,
    )
}

/// Collect `<col>` elements (and `display: table-column` elements) along with their
/// widths, which take precedence over cell widths in the fixed table layout algorithm.
/// Columns without a specified width are recorded as `auto`.
fn collect_columns(
    doc: &mut BaseDocument,
    node_id: NodeId,
    wm: WritingMode,
    columns: &mut Vec<TableColumn>,
    column_sizes: &mut Vec<TrackSizingFunction>,
) {
    let node = &doc.nodes[node_id];
    if !node.is_element() {
        return;
    }
    let Some(display) = node.display_style() else {
        return;
    };

    match display.inside() {
        DisplayInside::TableColumnGroup => {
            let first_column = columns.len();
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_columns(doc, child_id, wm, columns, column_sizes);
            }
            doc.nodes[node_id].children = children;
            for column in &mut columns[first_column..] {
                column.group.get_or_insert(node_id);
            }
        }
        DisplayInside::TableColumn => {
            let style = table_taffy_style(&node.primary_styles().unwrap(), wm);
            let span: u16 = node
                .attr(local_name!("span"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.max(1))
                .unwrap_or(1);
            let column: TrackSizingFunction = match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    // A definite `max-width` clamps the column width; a zero width is treated as auto
                    let mut width = style.size.width.value();
                    if style.max_size.width.into_raw().tag() == taffy::CompactLength::LENGTH_TAG {
                        width = width.min(style.max_size.width.into_raw().value());
                    }
                    if width > 0.0 {
                        style_helpers::length(width)
                    } else {
                        style_helpers::auto()
                    }
                }
                taffy::CompactLength::PERCENT_TAG => {
                    style_helpers::percent(style.size.width.value())
                }
                // Browsers treat calc() widths on columns as auto
                _ => style_helpers::auto(),
            };
            // A definite `min-width` on a column acts as a floor on its width
            let column = match style.min_size.width.into_raw().tag() {
                taffy::CompactLength::LENGTH_TAG if column.max.is_auto() => style_helpers::minmax(
                    style_helpers::length(style.min_size.width.into_raw().value()),
                    style_helpers::auto(),
                ),
                _ => column,
            };
            for _ in 0..span {
                columns.push(TableColumn {
                    node_id,
                    group: None,
                });
                column_sizes.push(column);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: NodeId,
    wm: WritingMode,
    is_fixed: bool,
    border_collapse: BorderCollapse,
    row: &mut u16,
    cursor: &mut ColumnCursor,
    cells: &mut Vec<TableCell>,
    oof_children: &mut Vec<(NodeId, taffy::Position)>,
    rows: &mut Vec<TableRow>,
    columns: &mut Vec<TrackSizingFunction>,
    percent_columns: &mut Vec<(u16, f32, f32)>,
) {
    let node = &mut doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.display_style() else {
        #[cfg(feature = "tracing")]
        tracing::info!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    // Absolutely positioned boxes are out-of-flow: they are laid out by their containing
    // block rather than as part of the table grid.
    let position = node.primary_styles().unwrap().clone_position();
    if position.is_absolutely_positioned() {
        oof_children.push((node_id, stylo_taffy::convert::position(position)));
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            let first_row = rows.len();
            // 行组之间 rowspan 不延续(HTML 表格模型:rowspan 截到所在行组的末行,
            // 见 `build_table_context`)。`display: contents` 不构成行组。
            if display.inside() != DisplayInside::Contents {
                cursor.rowspans.clear();
            }
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(
                    doc,
                    child_id,
                    wm,
                    is_fixed,
                    border_collapse,
                    row,
                    cursor,
                    cells,
                    oof_children,
                    rows,
                    columns,
                    percent_columns,
                );
            }
            doc.nodes[node_id].children = children;
            if display.inside() != DisplayInside::Contents {
                for row in &mut rows[first_row..] {
                    row.group.get_or_insert(node_id);
                }
            }
        }
        DisplayInside::TableRow => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            *row += 1;
            cursor.start_row();

            let height = node.primary_styles().and_then(|style| {
                let height = stylo_taffy::convert::dimension(&style.clone_height());
                (height.tag() == taffy::CompactLength::LENGTH_TAG).then(|| height.value())
            });
            let border = node.primary_styles().unwrap().clone_border();
            rows.push(TableRow {
                node_id,
                group: None,
                height,
                is_constrained: height.is_some(),
                border,
            });

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(
                    doc,
                    child_id,
                    wm,
                    is_fixed,
                    border_collapse,
                    row,
                    cursor,
                    cells,
                    oof_children,
                    rows,
                    columns,
                    percent_columns,
                );
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .unwrap_or(1)
                .max(1);
            let rowspan: u16 = node
                .attr(local_name!("rowspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.clamp(1, 65534))
                .unwrap_or(1);
            let mut style = table_taffy_style(stylo_style, wm);
            let border = stylo_style.clone_border();
            let col = cursor.next_free();

            // In the collapsed borders model the borders are laid out as gutters between
            // the cells (see the table's `gap`) rather than as part of the cells
            if border_collapse == BorderCollapse::Collapse {
                style.border = taffy::Rect::ZERO.map(style_helpers::length);
            }

            // In the fixed table layout algorithm the widths of columns are not
            // affected by cell contents, so cells must not impose a min-content
            // floor on their tracks.
            if is_fixed {
                style.min_size.width = style_helpers::length(0.0);
            }

            // Column widths from `<col>` elements take precedence over cell widths
            let col_needs_width =
                (col as usize) >= columns.len() || columns[col as usize].max.is_auto();
            // 跨列单元格的宽度不归属任何单独一列。
            if *row == 1 && colspan == 1 && col_needs_width {
                let column: TrackSizingFunction = match style.size.width.tag() {
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
                            let extra = match style.box_sizing {
                                taffy::BoxSizing::ContentBox => {
                                    let padding =
                                        style.padding.resolve_or_zero(None, resolve_calc_value);
                                    let border =
                                        style.border.resolve_or_zero(None, resolve_calc_value);
                                    padding.left + padding.right + border.left + border.right
                                }
                                taffy::BoxSizing::BorderBox => 0.0,
                            };
                            // Resolved in `build_table_context` once the column count is known
                            percent_columns.push((col, style.size.width.value(), extra));
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
                };
                if (col as usize) < columns.len() {
                    if !column.max.is_auto() {
                        columns[col as usize] = column;
                    }
                } else {
                    columns.resize(col as usize, style_helpers::auto());
                    columns.push(column);
                }
            }

            // The margin properties do not apply to table-internal elements
            style.margin = taffy::Rect::ZERO.map(style_helpers::length);

            // 网格位置(`grid_row` / `grid_column`)在 `build_table_context` 里按
            // collapse 与否统一写入,见 `grid_line`。
            if rowspan == 1 && style.size.height.tag() == taffy::CompactLength::LENGTH_TAG {
                if let Some(row) = rows.last_mut() {
                    row.is_constrained = true;
                }
            }

            style.size.width = style_helpers::auto();
            cells.push(TableCell {
                node_id,
                row: row.saturating_sub(1),
                column: col,
                rowspan,
                colspan,
                style,
                border,
            });

            cursor.place(colspan, rowspan);
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

impl taffy::TraversePartialTree for TableTreeWrapper<'_, '_> {
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
impl taffy::TraverseTree for TableTreeWrapper<'_, '_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_, '_> {
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

impl taffy::LayoutGridContainer for TableTreeWrapper<'_, '_> {
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
