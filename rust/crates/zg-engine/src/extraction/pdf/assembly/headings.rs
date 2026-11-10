use super::{Bookmarks, Kind, Page, text};
use crate::models::layout::LayoutKind;
use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

fn bookmark_key(s: &str) -> String {
    static P: LazyLock<Regex> = LazyLock::new(|| text::re(r"^\s*\d+(?:[.-]\d+)*\.?\s+"));
    text::norm(&P.replace(&text::clean(s), ""))
}
#[expect(
    clippy::cast_possible_truncation,
    reason = "Style and relative vertical bins have bounded PDF coordinates"
)]
#[expect(
    clippy::too_many_lines,
    reason = "Three-pass frozen heading inference with shared style evidence"
)]
pub(super) fn assign(pages: &mut [Page], bookmarks: &Bookmarks) {
    static FOLIO: LazyLock<Regex> = LazyLock::new(|| text::re(r"(?i)^(?:page\s*)?\d{1,5}$"));
    let mut repeated: BTreeMap<(String, i64), Vec<u32>> = BTreeMap::new();
    for pg in pages.iter() {
        for b in &pg.blocks {
            if b.bounds[3] < 0.12 * pg.height || b.bounds[1] > 0.88 * pg.height {
                let numbers = repeated
                    .entry((
                        text::norm(&b.text),
                        (b.bounds[1] / pg.height * 50.0).round_ties_even() as i64,
                    ))
                    .or_default();
                if !numbers.contains(&pg.number) {
                    numbers.push(pg.number);
                }
            }
        }
    }
    let mut styles: BTreeMap<i64, usize> = BTreeMap::new();
    let mut anchor: Vec<(i64, Vec<(usize, usize)>)> = Vec::new();
    for pg in pages.iter_mut() {
        let marks: BTreeMap<_, _> = bookmarks
            .get(&pg.number)
            .into_iter()
            .flatten()
            .filter_map(|(title, level)| {
                let key = bookmark_key(title);
                (!key.is_empty()).then_some((key, *level))
            })
            .collect();
        for b in &mut pg.blocks {
            let top = b.bounds[3] < 0.12 * pg.height;
            let bottom = b.bounds[1] > 0.88 * pg.height;
            if (top || bottom) && FOLIO.is_match(&text::clean(&b.text)) {
                b.kind = Kind::Folio;
            } else if (top || bottom)
                && b.text.chars().count() < 110
                && repeated
                    .get(&(
                        text::norm(&b.text),
                        (b.bounds[1] / pg.height * 50.0).round_ties_even() as i64,
                    ))
                    .is_some_and(|p| p.len() >= 3)
            {
                b.kind = if top { Kind::Header } else { Kind::Footer };
            } else if matches!(b.kind, Kind::Paragraph | Kind::Heading)
                && let Some(&level) = marks.get(&bookmark_key(&b.text))
            {
                b.kind = Kind::Heading;
                b.level = Some(level);
            }
            if b.kind == Kind::Heading {
                let style = (b.size * 2.0).round_ties_even() as i64;
                *styles.entry(style).or_default() += 1;
                let level = text::numbered(&b.text)
                    .or(b.level)
                    .or_else(|| (b.label == Some(LayoutKind::DocumentTitle)).then_some(1));
                if let Some(level) = level {
                    b.level = Some(level);
                    let i = anchor
                        .iter()
                        .position(|(s, _)| *s == style)
                        .unwrap_or_else(|| {
                            anchor.push((style, Vec::new()));
                            anchor.len() - 1
                        });
                    let counts = &mut anchor[i].1;
                    if let Some((_, count)) = counts.iter_mut().find(|(l, _)| *l == level) {
                        *count += 1;
                    } else {
                        counts.push((level, 1));
                    }
                }
            }
        }
    }
    let common: Vec<_> = styles
        .iter()
        .filter_map(|(&s, &n)| (n >= 2).then_some(s))
        .collect();
    for pg in pages {
        let mut local: Vec<_> = pg
            .blocks
            .iter()
            .filter(|b| b.kind == Kind::Heading)
            .map(|b| (b.size * 2.0).round_ties_even() as i64)
            .collect();
        local.sort_unstable();
        local.dedup();
        for b in &mut pg.blocks {
            if b.kind == Kind::Heading && b.level.is_none() {
                let style = (b.size * 2.0).round_ties_even() as i64;
                let ranked = if common.contains(&style) {
                    &common
                } else {
                    &local
                };
                b.level = Some(
                    anchor
                        .iter()
                        .find(|(s, _)| *s == style)
                        .and_then(|(_, counts)| {
                            counts
                                .iter()
                                .copied()
                                .reduce(|a, b| if b.1 > a.1 { b } else { a })
                                .map(|(level, _)| level)
                        })
                        .unwrap_or_else(|| {
                            (1 + ranked.iter().filter(|&&s| s > style).count()).min(6)
                        }),
                );
            }
        }
    }
}
