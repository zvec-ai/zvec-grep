//! Filter figure text while preserving native tables and page backgrounds.
use crate::models::layout::LayoutKind;
use liteparse::types::{Page, Rect};

#[derive(Clone, Copy)]
pub(super) struct Region {
    pub kind: LayoutKind,
    pub bounds: [f64; 4],
}

pub(super) fn rect(b: &Rect) -> [f64; 4] {
    [
        f64::from(b.x),
        f64::from(b.y),
        f64::from(b.x + b.width),
        f64::from(b.y + b.height),
    ]
}
pub(super) fn area(b: [f64; 4]) -> f64 {
    (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0)
}
fn overlap(a: [f64; 4], b: [f64; 4]) -> f64 {
    area([
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ])
}
fn figure(kind: LayoutKind) -> bool {
    matches!(
        kind,
        LayoutKind::Image | LayoutKind::Chart | LayoutKind::HeaderImage | LayoutKind::FooterImage
    )
}
fn owner(bounds: [f64; 4], regions: &[Region]) -> Option<usize> {
    let mut best = None;
    let mut score = -1.0;
    for (index, region) in regions.iter().enumerate() {
        let b = region.bounds;
        let coverage = overlap(bounds, b) / area(bounds).max(0.0001);
        let center = [bounds[0].midpoint(bounds[2]), bounds[1].midpoint(bounds[3])];
        let inside = (0..2).all(|j| center[j] >= b[j] && center[j] <= b[j + 2]);
        if coverage >= 0.5 || inside {
            let rank = coverage + if inside { 0.05 } else { 0.0 } + 0.00001 / (1.0 + area(b));
            if rank > score {
                score = rank;
                best = Some(index);
            }
        }
    }
    best
}

pub(super) fn apply(page: &mut Page, mut regions: Vec<Region>, tables: &[[f64; 4]]) {
    let w = f64::from(page.page_width);
    let h = f64::from(page.page_height);
    regions.retain(|r| {
        (r.kind != LayoutKind::Image || overlap(r.bounds, [0.0, 0.0, w, h]) < w * h * 0.8)
            && (!figure(r.kind)
                || area(r.bounds) <= 0.0
                || !tables
                    .iter()
                    .any(|t| overlap(*t, r.bounds) / area(r.bounds) >= 0.8))
    });
    page.text_items.retain(|item| {
        let bounds = [
            f64::from(item.x),
            f64::from(item.y),
            f64::from(item.x + item.width),
            f64::from(item.y + item.height),
        ];
        owner(bounds, &regions).is_none_or(|i| !figure(regions[i].kind))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse::types::TextItem;

    fn page() -> Page {
        let mut page: Page = serde_json::from_value(serde_json::json!({"page_number":1,"page_width":100.0,"page_height":100.0,"text_items":[]})).expect("page");
        page.text_items = vec![
            TextItem {
                text: "diagram noise".into(),
                x: 10.0,
                y: 10.0,
                width: 30.0,
                height: 10.0,
                ..TextItem::default()
            },
            TextItem {
                text: "Figure 1: caption".into(),
                x: 10.0,
                y: 55.0,
                width: 60.0,
                height: 10.0,
                ..TextItem::default()
            },
            TextItem {
                text: "Body paragraph".into(),
                x: 10.0,
                y: 75.0,
                width: 70.0,
                height: 10.0,
                ..TextItem::default()
            },
        ];
        page
    }

    #[test]
    fn figure_filter_keeps_caption_and_body_and_protects_native_table() {
        let region = Region {
            kind: LayoutKind::Image,
            bounds: [0.0, 0.0, 50.0, 50.0],
        };
        let mut filtered = page();
        apply(&mut filtered, vec![region], &[]);
        assert_eq!(
            filtered
                .text_items
                .iter()
                .map(|i| i.text.as_str())
                .collect::<Vec<_>>(),
            ["Figure 1: caption", "Body paragraph"]
        );
        let mut protected = page();
        apply(&mut protected, vec![region], &[[0.0, 0.0, 50.0, 50.0]]);
        assert_eq!(protected.text_items.len(), 3);
        let mut partial_table = page();
        apply(&mut partial_table, vec![region], &[[0.0, 0.0, 10.0, 10.0]]);
        assert_eq!(partial_table.text_items.len(), 2);
    }

    #[test]
    fn full_page_background_does_not_swallow_text() {
        let mut page = page();
        apply(
            &mut page,
            vec![Region {
                kind: LayoutKind::Image,
                bounds: [0.0, 0.0, 100.0, 100.0],
            }],
            &[],
        );
        assert_eq!(page.text_items.len(), 3);
    }
}
