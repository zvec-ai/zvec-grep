//! Cross-page candidate selection from the frozen R11–R13 assembly. Figures
//! affect flow selection without deleting or relabelling caption evidence.
use super::{Block, Kind, Page, same_size, text};
use regex::Regex;
use std::sync::LazyLock;

fn body(page: &Page) -> Vec<usize> {
    page.blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| (b.kind.body() && !b.text.trim().is_empty()).then_some(i))
        .collect()
}
fn rtl(c: char) -> bool {
    matches!(c,'\u{0590}'..='\u{08ff}'|'\u{fb1d}'..='\u{fdff}'|'\u{fe70}'..='\u{fefc}')
}
fn gutter(page: &Page, body: &[usize]) -> Option<f64> {
    let page_width = page.width;
    let long: Vec<_> = body
        .iter()
        .map(|&i| &page.blocks[i])
        .filter(|b| {
            b.kind == Kind::Paragraph
                && (0.19 * page_width..=0.50 * page_width).contains(&(b.first[2] - b.first[0]))
        })
        .collect();
    let left: Vec<_> = long
        .iter()
        .copied()
        .filter(|b| b.first[0] < 0.3 * page_width && b.first[2] < 0.61 * page_width)
        .collect();
    let right: Vec<_> = long
        .iter()
        .copied()
        .filter(|b| b.first[0] > 0.40 * page_width && b.first[2] > 0.69 * page_width)
        .collect();
    if left.len() < 2 || right.len() < 2 {
        return None;
    }
    let edge = |col: &[&Block], axis: usize| {
        let mut v: Vec<_> = col.iter().map(|b| b.first[axis]).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let left_edge = edge(&left, 2);
    let right_edge = edge(&right, 0);
    if !(0.008 * page_width..=0.15 * page_width).contains(&(right_edge - left_edge)) {
        return None;
    }
    let span = |col: &[&Block]| {
        (
            col.iter()
                .map(|b| b.bounds[1])
                .fold(f64::INFINITY, f64::min),
            col.iter()
                .map(|b| b.bounds[3])
                .fold(f64::NEG_INFINITY, f64::max),
        )
    };
    let (left_top, left_bottom) = span(&left);
    let (right_top, right_bottom) = span(&right);
    if left_bottom.min(right_bottom) - left_top.max(right_top) < 0.12 * page.height
        || left
            .iter()
            .chain(&right)
            .any(|b| b.text.chars().take(15).any(rtl))
    {
        return None;
    }
    Some(f64::midpoint(left_edge, right_edge))
}
fn frontier(page: &Page, body: &[usize], start: bool) -> usize {
    let order: Vec<_> = if start {
        body.to_vec()
    } else {
        body.iter().rev().copied().collect()
    };
    let first = order[0];
    let line = |b: &Block| if start { b.first } else { b.last };
    let first_line = line(&page.blocks[first]);
    let mid = page.width / 2.0;
    if start && first_line[0] < 0.88 * mid || !start && first_line[2] > 1.12 * mid {
        return first;
    }
    let Some(split) = gutter(page, body) else {
        return first;
    };
    for i in order {
        let block = &page.blocks[i];
        let l = line(block);
        if if start { l[0] >= split } else { l[2] <= split } {
            continue;
        }
        if i == first || l[0] < split && split < l[2] {
            return first;
        }
        for obj in &page.blocks {
            if !obj.kind.floating() {
                continue;
            }
            let b = obj.bounds;
            if l[2].min(b[2]) - l[0].max(b[0]) < 0.45 * (l[2] - l[0]) {
                continue;
            }
            if start && b[3] <= l[1] + block.size && b[1] <= first_line[1] + block.size
                || !start && b[1] >= l[3] - block.size && b[3] >= first_line[3] - block.size
            {
                return i;
            }
        }
        break;
    }
    first
}
fn non_prose(a: &Block, b: &Block) -> bool {
    static VERSION: LazyLock<Regex> = LazyLock::new(|| {
        text::re(
            r"(?i)^(?:(?:version|release)\s+|v\s*)?\d+(?:\.\d+){1,4}(?:[-+][\w.-]+|[a-z])?(?:\s*[(\[]\s*\d{2,4}[-./]\d{1,2}[-./]\d{1,4}[^)\]]{0,25}[)\]])?\s*[:.]?$",
        )
    });
    static LEADER: LazyLock<Regex> =
        LazyLock::new(|| text::re(r"(?:[.·…]\s*){3,}\d+(?:\s*[,–-]\s*\d+)*\s*$"));
    let a = text::clean(&a.text);
    let b = text::clean(&b.text);
    VERSION.is_match(&a)
        || VERSION.is_match(&b)
        || LEADER.is_match(
            &a.chars()
                .rev()
                .take(160)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>(),
        )
}
fn section_number(s: &str) -> bool {
    static P: LazyLock<Regex> = LazyLock::new(|| text::re(r"^\d+(?:\.\d+)+\s+[A-Z]"));
    P.is_match(s)
}
fn numeric(a: &Block, b: &Block) -> bool {
    static OPEN: LazyLock<Regex> = LazyLock::new(|| {
        text::re(
            r"(?i)\b(?:a|an|the|of|to|from|by|than|with|for|at|over|under|about|between|and|or|as|approximately|roughly|around|nearly|only|just)\s*$",
        )
    });
    static NUM: LazyLock<Regex> = LazyLock::new(|| {
        text::re(r"^\d+(?:[.,]\d+)*(?:[%°]|[-‐‑–][A-Za-z]+)?(?:\s+[A-Za-z]|[\u{3040}-\u{9fff}])")
    });
    static BARE: LazyLock<Regex> = LazyLock::new(|| text::re(r"^\d+\s+[A-Z]"));
    let left = a.text.trim_end();
    let right = b.text.trim_start();
    if left.is_empty()
        || right.is_empty()
        || text::closed(left)
        || !NUM.is_match(right)
        || section_number(right)
        || text::list(right)
        || b.bold > 0.45
    {
        return false;
    }
    if BARE.is_match(right)
        && right.split_whitespace().count() <= 8
        && b.last[1] - b.first[1] < 0.35 * b.size
        && !text::closed(right)
    {
        return false;
    }
    OPEN.is_match(left) || left.chars().last().is_some_and(text::cjk)
}
fn floats_before(page: &Page, b: &Block) -> bool {
    let [x, y, right, _] = b.first;
    let size = b.size.max(1.0);
    let width = right - x;
    let mut intervals = Vec::new();
    for obj in &page.blocks {
        let bounds = if obj.kind.floating() {
            obj.bounds
        } else {
            obj.first
        };
        if right.min(bounds[2]) - x.max(bounds[0]) < 0.45 * width || bounds[1] >= y {
            continue;
        }
        if obj.kind.floating() && bounds[3] <= y + size * 0.3 {
            intervals.push((bounds[1], bounds[3], obj.kind));
        } else if obj.kind.body() {
            return false;
        }
    }
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let Some(&(top, mut end, first)) = intervals.first() else {
        return false;
    };
    if top > 0.20 * page.height {
        return false;
    }
    let mut coverage = end - top;
    let mut object = matches!(first, Kind::Table | Kind::Figure);
    for (start, bottom, kind) in intervals.into_iter().skip(1) {
        if start - end > 3.5 * size {
            return false;
        }
        coverage += (bottom - end.max(start)).max(0.0);
        end = end.max(bottom);
        object |= matches!(kind, Kind::Table | Kind::Figure);
    }
    object && (0.0..=3.5 * size).contains(&(y - end)) && coverage >= 0.65 * (y - top)
}
fn caption_tail(page: &Page, b: &Block) -> bool {
    let size = b.size.max(1.0);
    page.blocks.iter().any(|c| {
        c.kind == Kind::Caption
            && !text::closed(&c.text)
            && (c.size - size).abs() <= 0.05 * size
            && (c.last[0] - b.first[0]).abs() <= 0.5 * size
            && (0.0..=0.5 * size).contains(&(b.first[1] - c.last[3]))
    })
}
fn continuation(a: &Block, b: &Block, left: &Page, right: &Page, extended: bool) -> bool {
    if a.kind != Kind::Paragraph
        || b.kind != Kind::Paragraph
        || a.section != b.section
        || !same_size(a, b)
    {
        return false;
    }
    if !(text::flow(&a.text, &b.text) || extended && numeric(a, b)) {
        return false;
    }
    let size = a.size.max(1.0);
    a.last[3] >= 0.70 * left.height
        && (b.first[1] <= 0.52 * right.height || extended && floats_before(right, b))
        && a.last[2] - a.last[0] >= (size * 8.0).max((a.bounds[2] - a.bounds[0]) * 0.42)
        && b.first[0] - b.bounds[0] <= 0.60 * size
        && b.first[0] <= 0.45 * right.width
}
fn legacy(
    left: &Page,
    right: &Page,
    left_body: &[usize],
    right_body: &[usize],
) -> Option<(usize, usize)> {
    if left_body.is_empty() || right_body.is_empty() || left.number + 1 != right.number {
        return None;
    }
    let a = frontier(left, left_body, false);
    let b = frontier(right, right_body, true);
    let l = &left.blocks[a];
    let r = &right.blocks[b];
    if non_prose(l, r) {
        return None;
    }
    (continuation(l, r, left, right, false)
        || !caption_tail(right, r) && continuation(l, r, left, right, true))
    .then_some((a, b))
}
fn caption_owner(page: &Page, b: &Block) -> bool {
    if b.kind != Kind::Paragraph
        || b.bold > 0.45
        || text::list(&b.text)
        || section_number(b.text.trim_start())
    {
        return false;
    }
    let size = b.size.max(1.0);
    if b.last[1] - b.first[1] > 1.5 * size || (b.last[0] - b.first[0]).abs() > 0.5 * size {
        return false;
    }
    page.blocks
        .iter()
        .filter(|c| {
            c.kind == Kind::Caption
                && c.section == b.section
                && (c.size - b.size).abs() <= 0.05 * size
                && (c.bold - b.bold).abs() <= 0.15
                && text::flow(&c.text, &b.text)
                && (0.0..=0.5 * size).contains(&(b.first[1] - c.last[3]))
                && (b.first[0] - c.last[0]).abs() <= 0.5 * size
                && b.bounds[0] >= c.bounds[0] - 0.5 * size
                && b.bounds[2] <= c.bounds[2] + 0.5 * size
        })
        .count()
        == 1
}
pub(super) fn boundary(left: &Page, right: &Page) -> Option<(usize, usize)> {
    let left_body = body(left);
    let mut right_body = body(right);
    let link = legacy(left, right, &left_body, &right_body);
    if link.is_some()
        || left_body.is_empty()
        || right_body.is_empty()
        || left.number + 1 != right.number
    {
        return link;
    }
    let outgoing = frontier(left, &left_body, false);
    let mut view = right.clone();
    while !right_body.is_empty() {
        let tail = frontier(&view, &right_body, true);
        if !caption_owner(&view, &view.blocks[tail]) {
            return None;
        }
        view.blocks[tail].kind = Kind::Caption;
        right_body.retain(|&i| i != tail);
        if right_body.is_empty() {
            return None;
        }
        let candidate = frontier(&view, &right_body, true);
        let c = &view.blocks[candidate];
        let t = &view.blocks[tail];
        if c.kind != Kind::Paragraph
            || c.section != t.section
            || c.first[1] < t.last[3]
            || (c.first[0] - t.first[0]).abs() > 0.6 * c.size.max(1.0)
            || text::list(&c.text)
        {
            return None;
        }
        if caption_owner(&view, c) {
            continue;
        }
        let a = left.blocks[outgoing].text.trim_end();
        let b = c.text.trim_start();
        if a.ends_with(['-', '‐'])
            && a.chars().rev().nth(1).is_some_and(char::is_alphabetic)
            && !b
                .chars()
                .next()
                .is_some_and(|ch| ch.is_lowercase() || text::cjk(ch))
        {
            return None;
        }
        return legacy(left, &view, &left_body, &right_body);
    }
    None
}
