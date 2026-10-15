//! `border-collapse: collapse` 的边框合并要逐条网格线段各自解析(CSS 2.1 §17.6.2)。
//!
//! 回归:整张表的边框早先统一取**第一个单元格**的 border。Word 导出的表格常把左上角
//! 表头格写成 `border:none`,于是整张表一条线都画不出来,而同一文档里首格有完整边框
//! 的表格却正常。反过来,把首格的边框套给全表也是错的——那格该留白的两条边会被画上。

use blitz_dom::node::SpecialElementData;
use blitz_dom::{CollapsedBorders, DocumentConfig};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

// 两张 Word 风格的表格,只差首个单元格:#none-first 的左上角是 `border:none`,
// #solid-first 的左上角有完整边框。
const HTML: &str = r#"<html><body style="margin:0">
    <table id="none-first" style="border-collapse:collapse;border:none">
        <tr>
            <td style="border:none">&nbsp;</td>
            <td style="border:solid black 1pt;border-left:none">%</td>
        </tr>
        <tr>
            <td style="border:solid black 1pt;border-top:none">Yes</td>
            <td style="border-top:none;border-left:none;border-bottom:solid black 1pt;border-right:solid black 1pt">91.7%</td>
        </tr>
    </table>
    <table id="solid-first" style="border-collapse:collapse;border:none">
        <tr>
            <td style="border:solid black 1pt">No</td>
            <td style="border:solid black 1pt;border-left:none">76.6%</td>
        </tr>
    </table>
</body></html>"#;

/// 表格 `id` 合并后的网格线。
fn collapsed_borders(doc: &HtmlDocument, id: &str) -> CollapsedBorders {
    let node_id = doc
        .query_selector(&format!("#{id}"))
        .unwrap()
        .unwrap_or_else(|| panic!("#{id} not found"));
    let node = doc.tree().get(node_id).unwrap();
    let SpecialElementData::TableRoot(table) = &node.element_data().unwrap().special_data else {
        panic!("#{id} did not become a table root");
    };
    table.collapsed_borders.clone()
}

/// 水平线 `line` 上逐列是否有线;垂直线同理逐行。
fn h_line(borders: &CollapsedBorders, line: usize) -> Vec<bool> {
    (0..borders.n_cols)
        .map(|col| borders.horizontal(line, col).is_some())
        .collect()
}

fn v_line(borders: &CollapsedBorders, line: usize) -> Vec<bool> {
    (0..borders.n_rows)
        .map(|row| borders.vertical(line, row).is_some())
        .collect()
}

#[test]
fn collapsed_border_ignores_a_borderless_first_cell() {
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let borders = collapsed_borders(&doc, "none-first");
    assert_eq!((borders.n_rows, borders.n_cols), (2, 2));

    // 顶边:左上角那格是 border:none,它头顶那段不画;右边那格有 border-top。
    assert_eq!(h_line(&borders, 0), vec![false, true], "表格顶边");
    // 行间线:第一列上方是 `border:none` 的角格、下方那格 `border-top:none`,两侧都没有
    // 边框,这一段就不该有线——正是逐段解析才能得出的结果(整表一套边框会把它画上)。
    // 第二列两侧都有边框,照画。
    assert_eq!(h_line(&borders, 1), vec![false, true], "行间线");
    assert_eq!(h_line(&borders, 2), vec![true, true], "表格底边");

    // 左边:第一行那格 border:none,第二行那格有 border-left。
    assert_eq!(v_line(&borders, 0), vec![false, true], "表格左边");
    // 列间线同理:第一行左右两格在这条线上都写了 none,该段不画;第二行两侧都有。
    assert_eq!(v_line(&borders, 1), vec![false, true], "列间线");
    assert_eq!(v_line(&borders, 2), vec![true, true], "表格右边");

    // 首格本就有完整边框的表:四周与内部都齐。
    let borders = collapsed_borders(&doc, "solid-first");
    assert_eq!(h_line(&borders, 0), vec![true, true]);
    assert_eq!(v_line(&borders, 0), vec![true]);
}

#[test]
fn hidden_beats_every_other_border() {
    const HTML: &str = r#"<html><body style="margin:0">
        <table id="t" style="border-collapse:collapse">
            <tr>
                <td style="border:solid black 5pt">a</td>
                <td style="border:solid black 1pt;border-left:hidden">b</td>
            </tr>
        </table>
    </body></html>"#;

    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let borders = collapsed_borders(&doc, "t");
    // 右格的 border-left:hidden 一票否决,尽管左格那边是 5pt 实线。
    assert!(
        borders.vertical(1, 0).is_none(),
        "hidden 必须让这一段线完全不画"
    );
    // 其余的线不受影响:外圈两条竖线各自来自自己那侧的单元格,左边那条明显更粗。
    let left = borders.vertical(0, 0).expect("左外框").width;
    let right = borders.vertical(2, 0).expect("右外框").width;
    assert!(left > right, "5pt 的左外框应比 1pt 的右外框宽 ({left} vs {right})");
}

#[test]
fn spanned_cells_have_no_line_running_through_them() {
    const HTML: &str = r#"<html><body style="margin:0">
        <table id="t" style="border-collapse:collapse">
            <tr>
                <td rowspan="2" style="border:solid black 1pt">tall</td>
                <td style="border:solid black 1pt">b1</td>
            </tr>
            <tr>
                <td style="border:solid black 1pt">b2</td>
            </tr>
        </table>
    </body></html>"#;

    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let borders = collapsed_borders(&doc, "t");
    assert_eq!((borders.n_rows, borders.n_cols), (2, 2));
    // 行间线穿不过 rowspan=2 的那格,只在右边一列上有。
    assert_eq!(h_line(&borders, 1), vec![false, true]);
}

/// 表格 `id` 的 taffy 行轨道数。
fn row_track_count(doc: &HtmlDocument, id: &str) -> usize {
    let node_id = doc.query_selector(&format!("#{id}")).unwrap().unwrap();
    let node = doc.tree().get(node_id).unwrap();
    let SpecialElementData::TableRoot(table) = &node.element_data().unwrap().special_data else {
        panic!("#{id} did not become a table root");
    };
    let grid_info = table.computed_grid_info.borrow();
    grid_info.as_ref().unwrap().rows.positions.len()
}

#[test]
fn rowspan_is_clamped_to_its_row_group() {
    // 末行的 rowspan=3 越过表格末尾、表头的 rowspan=2 越过 <thead>:HTML 都截到行组末行。
    // 不截的话越界部分生成隐式轨道,[线0, 行0, 线1, …] 的交错序列错位,整张表的线都画不出来;
    // 跨行组的那格还会把下一组首行的单元格挤到右边一列。
    const HTML: &str = r#"<html><body style="margin:0">
        <table id="t" style="border-collapse:collapse">
            <thead><tr>
                <th rowspan="2" style="border:solid black 1pt">h</th>
                <th style="border:solid black 1pt">h2</th>
            </tr></thead>
            <tbody>
                <tr>
                    <td style="border:solid black 1pt">a1</td>
                    <td style="border:solid black 1pt">a2</td>
                </tr>
                <tr>
                    <td rowspan="3" style="border:solid black 1pt">b1</td>
                    <td style="border:solid black 1pt">b2</td>
                </tr>
            </tbody>
        </table>
    </body></html>"#;

    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let borders = collapsed_borders(&doc, "t");
    assert_eq!((borders.n_rows, borders.n_cols), (3, 2));
    assert_eq!(
        row_track_count(&doc, "t"),
        2 * 3 + 1,
        "no implicit row tracks"
    );
    // 表头与表体之间的线两列都有:表头那格没有跨进表体。
    assert_eq!(h_line(&borders, 1), vec![true, true]);
    assert_eq!(h_line(&borders, 2), vec![true, true]);
}
