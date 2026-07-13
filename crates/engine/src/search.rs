use std::collections::HashMap;
use std::path::PathBuf;

use globset::Glob;

use crate::{
    CodeSymbolKind, Embedder, FileFormat, FileSegment, Result, Storage, storage::RecallHit,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    Lexical,
    Vector,
    #[default]
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub mode: SearchMode,
    pub filter: SearchFilter,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub path_glob: Option<String>,
    pub formats: Vec<FileFormat>,
    pub symbol_names: Vec<String>,
    pub symbol_kinds: Vec<CodeSymbolKind>,
    pub modified_after_ms: Option<u128>,
    pub modified_before_ms: Option<u128>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 10,
            mode: SearchMode::Hybrid,
            filter: SearchFilter::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Lexical,
    Vector,
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchResultItem {
    pub segment: FileSegment,
    pub score: f32,
    pub matched_by: MatchKind,
    pub evidence: SearchEvidence,
}

#[derive(Debug, Clone, Default)]
pub struct SearchEvidence {
    pub lexical_score: Option<f32>,
    pub lexical_rank: Option<usize>,
    pub vector_score: Option<f32>,
    pub vector_rank: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub items: Vec<SearchResultItem>,
    pub failures: Vec<SearchFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFailure {
    pub path: PathBuf,
    pub message: String,
}

pub(crate) fn search(
    embedder: &dyn Embedder,
    storage: &dyn Storage,
    request: SearchRequest,
) -> Result<SearchResult> {
    let query = request.query.trim();
    if query.is_empty() || request.limit == 0 {
        return Ok(SearchResult::default());
    }
    let total_segments = storage.stats()?.segments;
    let recall_limit = request
        .limit
        .saturating_mul(4)
        .max(request.limit)
        .min(total_segments);
    let lexical = if request.mode != SearchMode::Vector {
        adaptive_recall(
            recall_limit,
            total_segments,
            request.limit,
            &request.filter,
            |depth| storage.lexical_recall(query, depth),
        )?
    } else {
        Vec::new()
    };
    let vector = if request.mode != SearchMode::Lexical {
        let mut vectors = embedder.embed(&[query.to_owned()])?;
        if vectors.len() != 1 {
            return Err(crate::EngineError::Embedding(format!(
                "embedder returned {} vectors for one query",
                vectors.len()
            )));
        }
        let query_vector = vectors.remove(0);
        adaptive_recall(
            recall_limit,
            total_segments,
            request.limit,
            &request.filter,
            |depth| storage.vector_recall(&query_vector, depth),
        )?
    } else {
        Vec::new()
    };

    let mut items = match request.mode {
        SearchMode::Lexical => direct(lexical, MatchKind::Lexical),
        SearchMode::Vector => direct(vector, MatchKind::Vector),
        SearchMode::Hybrid => reciprocal_rank_fusion(lexical, vector),
    };
    items.truncate(request.limit);
    Ok(SearchResult {
        items,
        failures: Vec::new(),
    })
}

fn adaptive_recall(
    initial_depth: usize,
    max_depth: usize,
    wanted: usize,
    filter: &SearchFilter,
    mut recall: impl FnMut(usize) -> Result<Vec<RecallHit>>,
) -> Result<Vec<RecallHit>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }
    let mut depth = initial_depth.max(1);
    loop {
        let mut hits = recall(depth)?;
        let exhausted = hits.len() < depth;
        apply_filter(&mut hits, filter)?;
        if hits.len() >= wanted || exhausted || depth >= max_depth {
            return Ok(hits);
        }
        depth = depth.saturating_mul(2).min(max_depth);
    }
}

fn apply_filter(hits: &mut Vec<RecallHit>, filter: &SearchFilter) -> Result<()> {
    let path_matcher = filter
        .path_glob
        .as_ref()
        .map(|pattern| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| crate::EngineError::InvalidConfig(error.to_string()))
        })
        .transpose()?;
    hits.retain(|hit| {
        path_matcher
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(&hit.file.relative_path))
            && (filter.formats.is_empty() || filter.formats.contains(&hit.file.format))
            && (filter.symbol_names.is_empty()
                || hit.segment.symbol.as_ref().is_some_and(|symbol| {
                    filter
                        .symbol_names
                        .iter()
                        .any(|name| symbol.name.eq_ignore_ascii_case(name))
                }))
            && (filter.symbol_kinds.is_empty()
                || hit
                    .segment
                    .symbol
                    .as_ref()
                    .is_some_and(|symbol| filter.symbol_kinds.contains(&symbol.kind)))
            && filter
                .modified_after_ms
                .is_none_or(|time| hit.file.modified_ms >= time)
            && filter
                .modified_before_ms
                .is_none_or(|time| hit.file.modified_ms <= time)
    });
    Ok(())
}

fn direct(hits: Vec<RecallHit>, matched_by: MatchKind) -> Vec<SearchResultItem> {
    hits.into_iter()
        .enumerate()
        .map(|(rank, hit)| SearchResultItem {
            segment: hit.segment,
            score: hit.score,
            matched_by,
            evidence: match matched_by {
                MatchKind::Lexical => SearchEvidence {
                    lexical_score: Some(hit.score),
                    lexical_rank: Some(rank + 1),
                    ..SearchEvidence::default()
                },
                MatchKind::Vector => SearchEvidence {
                    vector_score: Some(hit.score),
                    vector_rank: Some(rank + 1),
                    ..SearchEvidence::default()
                },
                MatchKind::Hybrid => SearchEvidence::default(),
            },
        })
        .collect()
}

fn reciprocal_rank_fusion(
    lexical: Vec<RecallHit>,
    vector: Vec<RecallHit>,
) -> Vec<SearchResultItem> {
    const K: f32 = 60.0;
    let mut combined: HashMap<String, SearchResultItem> = HashMap::new();
    for (kind, hits) in [(MatchKind::Lexical, lexical), (MatchKind::Vector, vector)] {
        for (rank, hit) in hits.into_iter().enumerate() {
            let item = combined
                .entry(hit.segment.id.clone())
                .or_insert(SearchResultItem {
                    segment: hit.segment,
                    score: 0.0,
                    matched_by: kind,
                    evidence: SearchEvidence::default(),
                });
            item.score += 1.0 / (K + rank as f32 + 1.0);
            match kind {
                MatchKind::Lexical => {
                    item.evidence.lexical_score = Some(hit.score);
                    item.evidence.lexical_rank = Some(rank + 1);
                }
                MatchKind::Vector => {
                    item.evidence.vector_score = Some(hit.score);
                    item.evidence.vector_rank = Some(rank + 1);
                }
                MatchKind::Hybrid => {}
            }
            if item.matched_by != kind {
                item.matched_by = MatchKind::Hybrid;
            }
        }
    }
    let mut items: Vec<_> = combined.into_values().collect();
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queries_return_no_results() {
        let embedder = crate::DeterministicEmbedder::new(32).unwrap();
        let storage = crate::InMemoryStorage::new();
        assert!(
            search(&embedder, &storage, SearchRequest::new(" "))
                .unwrap()
                .items
                .is_empty()
        );
    }
}
