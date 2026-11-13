//! The frozen PP-S/native-text assembly used by the PDF `PoC`. Coordinates are
//! display-space points, top-left origin; this module does not load models.

mod flow;
mod headings;
mod page;
#[cfg(test)]
mod tests;
mod text;

use std::collections::BTreeMap;

use super::super::TextSource;
use super::PdfMarkdownSource;
use crate::{
    EngineError,
    domain::{FileFormat, PdfBlock, PdfSourceMap, PdfSourcePart, SourcePath},
    models::layout::LayoutKind,
};

pub(super) type Bounds = [f64; 4];
pub(super) type Bookmarks = BTreeMap<u32, Vec<(String, usize)>>;

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub text: String,
    pub bounds: Bounds,
    pub size: f64,
    pub bold: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Region {
    pub kind: LayoutKind,
    pub bounds: Bounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Paragraph,
    Heading,
    Figure,
    Caption,
    Table,
    Formula,
    Header,
    Footer,
    Folio,
    Footnote,
    List,
}

impl Kind {
    fn from_layout(label: LayoutKind) -> Self {
        use LayoutKind as L;
        match label {
            L::Heading | L::DocumentTitle => Self::Heading,
            L::Image | L::Chart | L::HeaderImage | L::FooterImage => Self::Figure,
            L::FigureCaption | L::TableCaption | L::ChartCaption => Self::Caption,
            L::Table => Self::Table,
            L::Formula | L::FormulaNumber => Self::Formula,
            L::Header => Self::Header,
            L::Footer => Self::Footer,
            L::PageNumber => Self::Folio,
            L::Footnote => Self::Footnote,
            _ => Self::Paragraph,
        }
    }
    fn body(self) -> bool {
        matches!(self, Self::Paragraph | Self::Heading | Self::List)
    }
    fn floating(self) -> bool {
        matches!(self, Self::Figure | Self::Table | Self::Caption)
    }
}

#[derive(Clone, Debug)]
struct Block {
    kind: Kind,
    region: Option<usize>,
    label: Option<LayoutKind>,
    text: String,
    bounds: Bounds,
    items: Vec<usize>,
    size: f64,
    bold: f64,
    first: Bounds,
    last: Bounds,
    level: Option<usize>,
    section: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct Page {
    pub number: u32,
    pub width: f64,
    pub height: f64,
    blocks: Vec<Block>,
}

pub(super) fn assemble_page(
    number: u32,
    width: f64,
    height: f64,
    items: &[Item],
    regions: Vec<Region>,
) -> Page {
    // A background photograph must not consume the page's overlaid native text.
    // This is the accepted 80% policy, applied before ownership assignment.
    let regions: Vec<_> = regions
        .into_iter()
        .filter(|r| {
            r.kind != LayoutKind::Image
                || area(&intersection(r.bounds, [0.0, 0.0, width, height])) < width * height * 0.80
        })
        .collect();
    Page {
        number,
        width,
        height,
        blocks: page::assemble(items, &regions, width),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Build one document and its source map together to preserve byte correspondence"
)]
pub(super) fn finish(
    mut pages: Vec<Page>,
    bookmarks: &Bookmarks,
    path: SourcePath,
) -> Result<PdfMarkdownSource, EngineError> {
    headings::assign(&mut pages, bookmarks);
    let mut stack: Vec<(usize, String)> = Vec::new();
    for page in &mut pages {
        for block in &mut page.blocks {
            if block.kind == Kind::Heading {
                let level = block.level.unwrap_or(1);
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, block.text.clone()));
            }
            block.section = stack.iter().map(|(_, s)| s.clone()).collect();
        }
    }
    let mut joins = BTreeMap::new();
    for i in 1..pages.len() {
        if let Some((a, b)) = flow::boundary(&pages[i - 1], &pages[i]) {
            joins.insert((i, b), (i - 1, a));
        }
    }
    let mut entities: Vec<(String, Vec<PdfSourcePart>)> = Vec::new();
    let mut owners = BTreeMap::new();
    for (page_index, page) in pages.iter().enumerate() {
        for (block_index, block) in page.blocks.iter().enumerate() {
            if block.kind == Kind::Figure || block.text.trim().is_empty() {
                continue;
            }
            let source = PdfBlock {
                page: page.number,
                id: u32::try_from(block_index + 1)
                    .map_err(|_| EngineError::invalid_argument("Too many PDF blocks"))?,
                bounds: normalize_bounds(block.bounds, page.width, page.height),
            };
            let owner = joins
                .get(&(page_index, block_index))
                .and_then(|key| owners.get(key))
                .copied();
            let index = if let Some(index) = owner {
                let (text, parts): &mut (String, Vec<PdfSourcePart>) = &mut entities[index];
                // Preserve each contributing page/block, including a clipped
                // trailing hyphen when a word continues on the next page.
                if text.ends_with('-') && block.text.starts_with(char::is_lowercase) {
                    text.pop();
                    if let Some(last) = parts.last_mut() {
                        last.range.end = text.len();
                    }
                } else if !text::adjacent_cjk(text, &block.text) {
                    text.push(' ');
                }
                let start = text.len();
                text.push_str(&block.text);
                parts.push(PdfSourcePart {
                    range: start..text.len(),
                    block: source,
                });
                index
            } else {
                let prefix = if block.kind == Kind::Heading {
                    format!("{} ", "#".repeat(block.level.unwrap_or(1).clamp(1, 6)))
                } else {
                    String::new()
                };
                let text = format!("{prefix}{}", block.text);
                let size = text.len();
                entities.push((
                    text,
                    vec![PdfSourcePart {
                        range: 0..size,
                        block: source,
                    }],
                ));
                entities.len() - 1
            };
            owners.insert((page_index, block_index), index);
        }
    }
    let mut markdown = String::new();
    let mut parts = Vec::new();
    for (text, spans) in entities {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        let offset = markdown.len();
        markdown.push_str(&text);
        parts.extend(
            spans
                .into_iter()
                .filter(|p| !p.range.is_empty())
                .map(|mut p| {
                    p.range.start += offset;
                    p.range.end += offset;
                    p
                }),
        );
    }
    let source_map = PdfSourceMap {
        markdown_start: 0,
        text_len: markdown.len(),
        parts,
    };
    if !markdown.is_empty() {
        source_map.validate(&markdown)?;
    }
    Ok(PdfMarkdownSource {
        text: TextSource {
            relative_path: path,
            formats: vec![FileFormat::Pdf],
            text: markdown,
        },
        source_map,
    })
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Clamped normalized coordinates are bounded to one million"
)]
fn normalize_bounds(b: Bounds, width: f64, height: f64) -> Option<[u32; 4]> {
    let bounds = std::array::from_fn(|i| {
        ((b[i] / if i % 2 == 0 { width } else { height }).clamp(0.0, 1.0) * 1_000_000.0).round()
            as u32
    });
    // Keep text with unknown font advance or entirely outside the visible
    // crop; do not invent a nonzero rectangle merely to satisfy validation.
    (bounds[0] < bounds[2] && bounds[1] < bounds[3]).then_some(bounds)
}
fn area(b: &Bounds) -> f64 {
    (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0)
}
fn intersection(a: Bounds, b: Bounds) -> Bounds {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}
fn union(a: Bounds, b: Bounds) -> Bounds {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.len().is_multiple_of(2) {
        f64::midpoint(v[v.len() / 2 - 1], v[v.len() / 2])
    } else {
        v[v.len() / 2]
    }
}
fn same_size(a: &Block, b: &Block) -> bool {
    (a.size - b.size).abs() <= 0.7_f64.max(a.size.min(b.size) * 0.1)
}
