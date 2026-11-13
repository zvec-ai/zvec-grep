use super::{
    Block, Bounds, Item, Kind, Region, area, intersection, median, same_size, text, union,
};
use crate::models::layout::LayoutKind;
use unicode_categories::UnicodeCategories;

struct Line {
    text: String,
    bounds: Bounds,
    ids: Vec<usize>,
    size: f64,
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the frozen page-assembly decision order auditable"
)]
pub(super) fn assemble(items: &[Item], regions: &[Region], width: f64) -> Vec<Block> {
    if items.is_empty() {
        return Vec::new();
    }
    // First-encounter order matches the frozen assembler's ownership buckets.
    let mut owned: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut best = None;
        let mut score = -1.0;
        for (r, region) in regions.iter().enumerate() {
            let b = region.bounds;
            let coverage = area(&intersection(item.bounds, b)) / area(&item.bounds).max(0.0001);
            let center = [
                f64::midpoint(item.bounds[0], item.bounds[2]),
                f64::midpoint(item.bounds[1], item.bounds[3]),
            ];
            let inside = (0..2).all(|j| center[j] >= b[j] && center[j] <= b[j + 2]);
            if coverage >= 0.5 || inside {
                let rank = coverage + if inside { 0.05 } else { 0.0 } + 0.00001 / (1.0 + area(&b));
                if rank > score {
                    score = rank;
                    best = Some(r);
                }
            }
        }
        if let Some((_, ids)) = owned.iter_mut().find(|(r, _)| *r == best) {
            ids.push(i);
        } else {
            owned.push((best, vec![i]));
        }
    }
    let body = typography(items);
    let mut blocks = Vec::new();
    for (region, ids) in owned {
        let label = region.map(|i| regions[i].kind);
        let kind = label.map_or(Kind::Paragraph, Kind::from_layout);
        let rows = lines(items, ids, region.is_none());
        for group in groups(rows, kind, region, items) {
            let mut block = make_block(&group, kind, region, label, items);
            let s = block.text.as_str();
            if matches!(kind, Kind::Paragraph | Kind::Heading) && !s.is_empty() {
                if text::bullet(s) || kind == Kind::Paragraph && text::enumeration(s) {
                    block.kind = Kind::List;
                } else if kind == Kind::Heading
                    && block.bold < 0.25
                    && block.size <= body * 1.04
                    && s.starts_with(char::is_lowercase)
                    && text::closed(s)
                    && text::numbered(s).is_none()
                {
                    block.kind = Kind::Paragraph;
                } else if kind == Kind::Paragraph
                    && text::formula_number(s)
                    && regions.iter().any(|r| {
                        Kind::from_layout(r.kind) == Kind::Formula
                            && r.bounds[1] <= f64::midpoint(block.bounds[1], block.bounds[3])
                            && f64::midpoint(block.bounds[1], block.bounds[3]) <= r.bounds[3]
                            && (0.0..width * 0.28).contains(&(block.bounds[0] - r.bounds[2]))
                    })
                {
                    block.kind = Kind::Formula;
                } else if kind == Kind::Paragraph && text::caption(s) {
                    block.kind = Kind::Caption;
                } else if kind == Kind::Paragraph
                    && group.len() <= 3
                    && s.chars().count() <= 145
                    && !text::closed(s)
                    && (block.size >= body * 1.27
                        || block.bold >= 0.75 && block.size >= body * 1.09)
                    && s.chars().any(char::is_alphabetic)
                {
                    block.kind = Kind::Heading;
                }
            }
            blocks.push(block);
        }
    }
    let mut blocks = combine(order(blocks, width));
    for b in &mut blocks {
        let symbols: Vec<_> = b.text.chars().filter(|c| !c.is_whitespace()).collect();
        if b.region.is_some()
            || b.kind != Kind::Paragraph
            || symbols.is_empty()
            || symbols.len() > 8
            || !symbols.iter().all(|c| c.is_symbol())
        {
            continue;
        }
        let touched: Vec<_> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                b.items
                    .iter()
                    .any(|&i| area(&intersection(items[i].bounds, r.bounds)) > 0.0)
            })
            .collect();
        if let [(index, r)] = touched.as_slice()
            && matches!(r.kind, LayoutKind::Image | LayoutKind::Chart)
            && b.items.iter().all(|&i| edge(items[i].bounds, r.bounds))
        {
            b.kind = Kind::Figure;
            b.region = Some(*index);
        }
    }
    blocks
}

#[expect(
    clippy::float_cmp,
    reason = "Font styles are quantized to exact half points"
)]
fn typography(items: &[Item]) -> f64 {
    let mut counts: Vec<(f64, usize)> = Vec::new();
    for i in items {
        let size = (i.size * 2.0).round_ties_even() / 2.0;
        let n = i.text.trim().chars().count().min(100);
        if let Some((_, count)) = counts.iter_mut().find(|(s, _)| *s == size) {
            *count += n;
        } else {
            counts.push((size, n));
        }
    }
    counts
        .into_iter()
        .reduce(|a, b| if b.1 > a.1 { b } else { a })
        .map_or(10.0, |x| x.0)
}
fn lines(items: &[Item], mut ids: Vec<usize>, split: bool) -> Vec<Line> {
    ids.sort_by(|&a, &b| {
        items[a].bounds[3]
            .total_cmp(&items[b].bounds[3])
            .then_with(|| items[a].bounds[0].total_cmp(&items[b].bounds[0]))
    });
    let mut rows: Vec<(f64, Vec<usize>)> = Vec::new();
    for i in ids {
        if rows.last().is_none_or(|(y, _)| {
            (*y - items[i].bounds[3]).abs() > 1.0_f64.max(items[i].size * 0.28)
        }) {
            rows.push((items[i].bounds[3], Vec::new()));
        }
        rows.last_mut().expect("created row").1.push(i);
    }
    let mut result = Vec::new();
    for (_, mut ids) in rows {
        ids.sort_by(|&a, &b| items[a].bounds[0].total_cmp(&items[b].bounds[0]));
        let mut row = Vec::new();
        for i in ids {
            if split
                && row.last().is_some_and(|&last: &usize| {
                    items[i].bounds[0] - items[last].bounds[2] > 12.0_f64.max(1.8 * items[i].size)
                })
            {
                result.push(make_line(items, std::mem::take(&mut row)));
            }
            row.push(i);
        }
        if !row.is_empty() {
            result.push(make_line(items, row));
        }
    }
    result
}
fn make_line(items: &[Item], ids: Vec<usize>) -> Line {
    let first = &items[ids[0]];
    let mut value = first.text.clone();
    let mut bounds = first.bounds;
    for pair in ids.windows(2) {
        let a = &items[pair[0]];
        let b = &items[pair[1]];
        if let (Some(left), Some(right)) = (value.chars().last(), b.text.chars().next())
            && !left.is_whitespace()
            && !right.is_whitespace()
            && !",.;:!?)]}".contains(right)
            && !"([{".contains(left)
            && !(text::cjk(left) && text::cjk(right))
            && b.bounds[0] - a.bounds[2] >= 0.12 * a.size.max(b.size)
        {
            value.push(' ');
        }
        value.push_str(&b.text);
        bounds = union(bounds, b.bounds);
    }
    Line {
        text: value.trim().to_owned(),
        bounds,
        size: median(ids.iter().map(|&i| items[i].size).collect()),
        ids,
    }
}
fn groups(rows: Vec<Line>, kind: Kind, index: Option<usize>, items: &[Item]) -> Vec<Vec<Line>> {
    if index.is_none() {
        return rows.into_iter().map(|r| vec![r]).collect();
    }
    if !matches!(kind, Kind::Paragraph | Kind::Heading) {
        return vec![rows];
    }
    let left = rows
        .iter()
        .map(|r| r.bounds[0])
        .fold(f64::INFINITY, f64::min);
    let deltas: Vec<_> = rows
        .windows(2)
        .map(|r| r[1].bounds[1] - r[0].bounds[1])
        .filter(|d| *d > 0.0)
        .collect();
    let pitch = if deltas.len() >= 2 {
        median(deltas)
    } else {
        0.0
    };
    let mut result: Vec<Vec<Line>> = Vec::new();
    for row in rows {
        let split = result.last().is_none_or(|group| {
            let prev = group.last().expect("nonempty group");
            let size = row.size.max(1.0);
            let y = row.bounds[1] - prev.bounds[1];
            if kind == Kind::Heading {
                text::numbered(&row.text).is_some() && text::numbered(&group[0].text).is_some()
            } else {
                text::list(&row.text)
                    || (row.ids.iter().all(|&i| items[i].bold)
                        && !prev.ids.iter().any(|&i| items[i].bold)
                        && text::closed(&prev.text)
                        && y > 1.4 * size)
                    || y > (1.68 * size).max(1.23 * pitch)
                    || (!text::list(&group[0].text)
                        && row.bounds[0] - left > 0.65 * size
                        && row.bounds[0] - prev.bounds[0] > 0.55 * size)
            }
        });
        if split {
            result.push(Vec::new());
        }
        result.last_mut().expect("created group").push(row);
    }
    result
}
#[expect(
    clippy::cast_precision_loss,
    reason = "Text weights are bounded by input size"
)]
fn make_block(
    rows: &[Line],
    kind: Kind,
    region: Option<usize>,
    label: Option<LayoutKind>,
    items: &[Item],
) -> Block {
    let ids: Vec<_> = rows.iter().flat_map(|r| r.ids.iter().copied()).collect();
    let n: usize = ids.iter().map(|&i| items[i].text.chars().count()).sum();
    let bold: usize = ids
        .iter()
        .filter(|&&i| items[i].bold)
        .map(|&i| items[i].text.chars().count())
        .sum();
    let mut value = String::new();
    for row in rows {
        text::join(&mut value, &row.text);
    }
    Block {
        kind,
        region,
        label,
        text: value,
        bounds: rows
            .iter()
            .map(|r| r.bounds)
            .reduce(union)
            .expect("nonempty lines"),
        items: ids,
        size: (median(rows.iter().map(|r| r.size).collect()) * 100.0).round_ties_even() / 100.0,
        bold: bold as f64 / n.max(1) as f64,
        first: rows[0].bounds,
        last: rows.last().expect("nonempty lines").bounds,
        level: None,
        section: Vec::new(),
    }
}
fn join(a: &mut Block, b: Block) {
    text::join(&mut a.text, &b.text);
    a.bounds = union(a.bounds, b.bounds);
    a.items.extend(b.items);
    a.last = b.last;
}
fn combine(blocks: Vec<Block>) -> Vec<Block> {
    let mut result: Vec<Block> = Vec::new();
    for b in blocks {
        if let Some(a) = result
            .last_mut()
            .filter(|_| matches!(b.kind, Kind::Paragraph | Kind::Heading | Kind::List))
        {
            let [x, y, _, _] = b.first;
            let [left, top, right, bottom] = a.last;
            let size = b.size.max(1.0);
            let dx = x - right;
            let gap = y - bottom;
            let same_line = (y - top).abs() <= size * 0.3 && (-1.0..=size * 2.0).contains(&dx);
            if same_line
                && (b.kind == Kind::Heading && text::number_only(&a.text)
                    || a.kind == Kind::List && a.text.trim().chars().count() <= 3)
            {
                a.kind = if b.kind == Kind::Heading {
                    Kind::Heading
                } else {
                    Kind::List
                };
                a.label = b.label;
                join(a, b);
                continue;
            }
            let unmatched = a.region.is_none() && b.region.is_none();
            if unmatched
                && b.kind == Kind::Heading
                && text::short_number(&a.text)
                && a.size >= size * 1.5
                && a.bounds[0] < x
                && dx < size * 5.0
                && a.bounds[1] <= y
                && y <= a.bounds[3]
            {
                a.kind = Kind::Heading;
                a.size = b.size;
                a.label = b.label;
                join(a, b);
                continue;
            }
            if same_size(a, &b) {
                let headings = a.kind == Kind::Heading
                    && b.kind == Kind::Heading
                    && unmatched
                    && (left - x).abs() < 0.6 * size
                    && (0.0..=0.65 * size).contains(&gap)
                    && text::numbered(&b.text).is_none()
                    && (b.text.starts_with(char::is_lowercase)
                        || a.text.ends_with([',', '-', '，']));
                let caption = a.kind == Kind::Caption
                    && b.kind == Kind::Paragraph
                    && unmatched
                    && (left - x).abs() < 0.65 * size
                    && (0.0..=0.55 * size).contains(&gap)
                    && !text::closed(&a.text);
                let list = a.kind == Kind::List
                    && b.kind == Kind::Paragraph
                    && (0.0..=size * 0.9).contains(&gap)
                    && x - a.bounds[0] > size * 0.45
                    && x - a.bounds[0] <= size * 4.0
                    && (b.region.is_none() || text::bullet(&a.text) && a.text.chars().count() < 32);
                let omitted = a.kind == Kind::Paragraph
                    && b.kind == Kind::Paragraph
                    && unmatched
                    && (a.bounds[0] - x).abs() < size * 1.3
                    && (0.0..=size * 0.55).contains(&gap)
                    && !text::closed(&a.text)
                    && a.text.trim().chars().count() > 8;
                let column = a.kind == Kind::Paragraph
                    && b.kind == Kind::Paragraph
                    && x > a.bounds[2]
                    && y < a.last[1] - size * 4.0
                    && text::flow(&a.text, &b.text)
                    && ((b.bounds[2] - b.bounds[0]) - (a.bounds[2] - a.bounds[0])).abs()
                        < size * 4.0;
                if headings || caption || list || omitted || column {
                    join(a, b);
                    continue;
                }
            }
        }
        result.push(b);
    }
    result
}
fn gaps(blocks: &[Block], axis: usize) -> Vec<(f64, f64)> {
    let mut intervals: Vec<_> = blocks
        .iter()
        .map(|b| (b.bounds[axis], b.bounds[axis + 2]))
        .collect();
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let mut high = intervals[0].1;
    let mut out = Vec::new();
    for (a, b) in intervals.into_iter().skip(1) {
        if a > high {
            out.push((a - high, f64::midpoint(high, a)));
        }
        high = high.max(b);
    }
    out
}
fn order(blocks: Vec<Block>, width: f64) -> Vec<Block> {
    let mut stack = vec![blocks];
    let mut out = Vec::new();
    while let Some(mut group) = stack.pop() {
        if group.len() < 2 {
            out.extend(group);
            continue;
        }
        let maxgap = |v: Vec<(f64, f64)>| {
            v.into_iter()
                .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)))
        };
        let vertical: Vec<_> = gaps(&group, 0)
            .into_iter()
            .filter(|(gap, _)| *gap >= 8.0_f64.max(0.018 * width))
            .collect();
        let split = maxgap(vertical)
            .map(|(_, cut)| (0, cut))
            .or_else(|| maxgap(gaps(&group, 1)).map(|(_, cut)| (1, cut)));
        if let Some((axis, cut)) = split {
            let (lo, hi) = group.into_iter().partition(|b| b.bounds[axis] < cut);
            stack.push(hi);
            stack.push(lo);
        } else {
            group.sort_by(|a, b| {
                a.bounds[1]
                    .total_cmp(&b.bounds[1])
                    .then_with(|| a.bounds[0].total_cmp(&b.bounds[0]))
            });
            out.extend(group);
        }
    }
    out
}
fn edge(b: Bounds, figure: Bounds) -> bool {
    (0..2).any(|axis| {
        let other = 1 - axis;
        if b[other] < figure[other] || b[other + 2] > figure[other + 2] {
            return false;
        }
        let thickness = b[axis + 2] - b[axis];
        let overlap = b[axis + 2].min(figure[axis + 2]) - b[axis].max(figure[axis]);
        let center = f64::midpoint(b[axis], b[axis + 2]);
        let outside = (figure[axis] - center).max(center - figure[axis + 2]);
        thickness > 0.0
            && overlap >= thickness / 4.0
            && (0.0..=1.5_f64.min(thickness / 4.0)).contains(&outside)
    })
}
