//! Rerank recalled code within the positions assigned to code by fusion.
mod paths;
mod query;
mod recall;
mod source;
mod support;

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
};

use super::{
    pipeline::{ResolvedSearchRoute, SearchHit},
    stage_trace::StageTraceCollector,
};
use crate::{
    api::context::{options::QueryFilter, result::SearchRankingTrace},
    domain::{Content, ContentKind, EntityMetadata, FileId, Range},
};
use paths::Roles;
use query::{QueryFeatures, SourceLookup};

const FILE_DECAY: f64 = 0.50;

/// Internal policy values also provide a fixed-pool ablation seam for tests.
#[derive(Clone, Copy)]
struct Policy {
    name: &'static str,
    path_weight: f64,
    weak_names: bool,
    definition_intent: bool,
    supplemental_factor: f64,
    support_weight: f64,
    bonus_cap: f64,
}

impl Policy {
    const CURRENT: Self = Self {
        name: "code-rules",
        path_weight: 0.35,
        weak_names: true,
        definition_intent: true,
        supplemental_factor: 0.5,
        support_weight: 0.15,
        bonus_cap: 1.25,
    };

    #[cfg(test)]
    const BASELINE: Self = Self {
        name: "code-rules-baseline",
        path_weight: 0.0,
        weak_names: false,
        definition_intent: false,
        supplemental_factor: 1.0,
        support_weight: 0.0,
        bonus_cap: 0.50,
    };
}

struct Scored {
    hit: SearchHit,
    base: f64,
    raw_base: f64,
    adjusted_route_ranks: BTreeMap<String, usize>,
    static_score: f64,
    symbol_match: &'static str,
    symbol_bonus: f64,
    definition_bonus: f64,
    supplemental_factor: Option<f64>,
    path_bonus: f64,
    support_bonus: f64,
    support_count: u32,
    roles: Roles,
    overrides: Roles,
}

/// Rank without widening recall, inspecting source files, or loading a model.
pub(super) fn rank(
    hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    limit: usize,
    observation: Option<&mut StageTraceCollector>,
) -> Vec<SearchHit> {
    rank_with_policy(
        hits,
        routes,
        filter,
        maximum_rrf,
        limit,
        Policy::CURRENT,
        observation,
    )
}

fn rank_with_policy(
    mut hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    limit: usize,
    policy: Policy,
    observation: Option<&mut StageTraceCollector>,
) -> Vec<SearchHit> {
    let result_count = limit.min(hits.len());
    let is_code = |hit: &SearchHit| hit.entity.content().kind() == ContentKind::Code;
    // Stage diagnostics also report static ranks below the result cutoff.
    let slot_count = if observation.is_some() {
        hits.len()
    } else {
        result_count
    };
    let code_ranks: Vec<_> = hits
        .iter()
        .take(slot_count)
        .enumerate()
        .filter_map(|(index, hit)| is_code(hit).then_some(index + 1))
        .collect();

    // Only the visible positions are fixed. Any recalled code hit can fill them.
    let mut ranked_code = Vec::new();
    let visible = code_ranks.partition_point(|rank| *rank <= result_count);
    if visible > 0 {
        let code = hits.extract_if(.., |hit| is_code(hit)).collect();
        let (query, scored) = score_code(code, routes, filter, maximum_rrf, policy);
        if let Some(observation) = observation {
            let mut order = scored.iter().collect::<Vec<_>>();
            order.sort_by(|a, b| compare(a, b, a.static_score, b.static_score));
            for (item, &rank) in order.into_iter().zip(&code_ranks) {
                observation.static_rank(&item.hit, rank);
            }
        }
        ranked_code = select(scored, &query, &code_ranks[..visible], policy);
    }
    let mut ranked_code = ranked_code.into_iter().peekable();
    let mut documents = hits.into_iter();
    (1..=result_count)
        .map(|rank| {
            let mut hit = ranked_code
                .next_if(|hit| hit.rank == rank)
                .or_else(|| documents.next())
                .expect("each result position has a loaded hit");
            hit.rank = rank;
            if let Some(trace) = &mut hit.trace {
                trace.final_selection.returned_by_limit = true;
                trace.final_selection.cutoff_rank = limit;
            }
            hit
        })
        .collect()
}

fn score_code(
    hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    policy: Policy,
) -> (QueryFeatures, Vec<Scored>) {
    let mut queries = routes.iter().map(|r| r.query.as_str()).collect::<Vec<_>>();
    queries.sort_unstable();
    queries.dedup();
    let query_text = queries.join("\n");
    let query = QueryFeatures::new(&query_text);
    let supplemental_ranks =
        recall::SupplementalRanks::for_query(&hits, &query.source_lookup, &query_text);
    let literal_phrase = source::literal_phrase(&query_text);
    let competes_with_symbols = hits.iter().any(|hit| {
        matches!(hit.entity.metadata(), Some(EntityMetadata::Code(code)) if code.symbol_name.is_some())
    });
    let mut overrides = Roles::overrides(&query, filter);
    let roles = hits
        .iter()
        .map(|hit| Roles::from_path(&hit.file.relative_path().display().to_string()))
        .collect::<Vec<_>>();
    // A common dependency prior carries no information within this search scope.
    overrides.dependency |= roles.iter().all(|r| r.dependency);
    let mut scored = hits
        .into_iter()
        .zip(roles)
        .map(|(hit, roles)| {
            let mut overrides = overrides;
            if roles.tests
                && !overrides.tests
                && literal_phrase.is_some_and(|phrase| source::has_literal_argument(&hit, phrase))
            {
                overrides.tests = true;
            }
            let (symbol_match, symbol_bonus, definition_bonus) = match hit.entity.metadata() {
                Some(EntityMetadata::Code(code)) => query.symbol_bonus_with(code, policy),
                _ => ("none", 0.0, 0.0),
            };
            let supplemental_factor = match hit.entity.metadata() {
                Some(EntityMetadata::Code(code))
                    if code.is_supplemental && code.symbol_name.is_none() =>
                {
                    Some(
                        if competes_with_symbols
                            && !matches_source_lookup(
                                &hit,
                                code.signature.as_deref(),
                                &query.source_lookup,
                                &query_text,
                            )
                        {
                            policy.supplemental_factor
                        } else {
                            1.0
                        },
                    )
                }
                _ => None,
            };
            let raw_base = hit.score / maximum_rrf;
            let (ranking_rrf, adjusted_route_ranks) = supplemental_ranks.adjust(&hit);
            let base = ranking_rrf / maximum_rrf;
            let mut path_bonus = policy.path_weight
                * query.path_affinity(&hit.file.relative_path().display().to_string());
            // Weak lexical coverage is one signal even when repeated in the path.
            if symbol_match == "tokens" {
                path_bonus = (path_bonus - symbol_bonus).max(0.0);
            }
            let static_score = (base
                + (symbol_bonus + definition_bonus + path_bonus).min(policy.bonus_cap))
                * roles.factor(overrides)
                * supplemental_factor.unwrap_or(1.0);
            Scored {
                hit,
                base,
                raw_base,
                adjusted_route_ranks,
                static_score,
                symbol_match,
                symbol_bonus,
                definition_bonus,
                supplemental_factor,
                path_bonus,
                support_bonus: 0.0,
                support_count: 0,
                roles,
                overrides,
            }
        })
        .collect::<Vec<_>>();
    if policy.support_weight > 0.0 {
        support::apply(&mut scored, policy.support_weight);
    }
    (query, scored)
}

/// Recognize unquoted source pasted into search without reading the whole
/// entity. The leading recall evidence identifies the best matching fragment.
fn matches_source_lookup(
    hit: &SearchHit,
    binding: Option<&str>,
    lookup: &SourceLookup,
    query: &str,
) -> bool {
    let (query, whole_identifier) = match lookup {
        SourceLookup::Literal => return true,
        SourceLookup::Identifier(name) if name.is_ascii() || binding == Some(name.as_str()) => {
            return true;
        }
        // Without whitespace, a Unicode identifier can also read like a sentence.
        SourceLookup::Identifier(name) => (name.as_str(), true),
        SourceLookup::General => (query.trim(), false),
    };
    if query.is_empty() || query.len() > source::MAX_BYTES {
        return false;
    }
    let Some(fragment) = source::best_fragment(hit) else {
        return false;
    };
    let searchable = &fragment[..fragment.floor_char_boundary(source::MAX_BYTES)];
    if !whole_identifier {
        return searchable.contains(query);
    }
    let identifier_char = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '$');
    searchable.match_indices(query).any(|(start, _)| {
        !fragment[..start]
            .chars()
            .next_back()
            .is_some_and(identifier_char)
            && !fragment[start + query.len()..]
                .chars()
                .next()
                .is_some_and(identifier_char)
    })
}

fn compare(a: &Scored, b: &Scored, a_score: f64, b_score: f64) -> Ordering {
    b_score
        .total_cmp(&a_score)
        .then_with(|| a.hit.rank.cmp(&b.hit.rank))
        .then_with(|| a.hit.entity.id().as_str().cmp(b.hit.entity.id().as_str()))
}

fn select(
    mut scored: Vec<Scored>,
    query: &QueryFeatures,
    code_ranks: &[usize],
    policy: Policy,
) -> Vec<SearchHit> {
    let single_file = scored
        .iter()
        .all(|s| s.hit.file.id() == scored[0].hit.file.id());
    let mut selected_per_file = HashMap::<FileId, u32>::new();
    let mut output = Vec::with_capacity(code_ranks.len());
    for &rank in code_ranks {
        let effective = |item: &Scored| {
            let count = if single_file {
                0
            } else {
                selected_per_file
                    .get(&item.hit.file.id())
                    .copied()
                    .unwrap_or(0)
            };
            item.static_score / (1.0 + FILE_DECAY * f64::from(count))
        };
        let best = scored
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| compare(a, b, effective(a), effective(b)))
            .map_or(0, |(index, _)| index);
        let mut item = scored.swap_remove(best);
        let count = selected_per_file.entry(item.hit.file.id()).or_default();
        let factor = if single_file {
            1.0
        } else {
            1.0 / (1.0 + FILE_DECAY * f64::from(*count))
        };
        item.hit.score = item.static_score * factor;
        item.hit.rank = rank;
        if let Some(trace) = item.hit.trace.as_mut() {
            trace.ranking = Some(SearchRankingTrace {
                policy: policy.name.to_owned(),
                query_features: query.describe(),
                normalized_base: item.raw_base,
                adjusted_route_ranks: item.adjusted_route_ranks,
                symbol_match: item.symbol_match.to_owned(),
                symbol_bonus: item.symbol_bonus,
                definition_bonus: item.definition_bonus,
                supplemental_factor: item.supplemental_factor,
                path_bonus: item.path_bonus,
                file_support_bonus: item.support_bonus,
                file_support_count: item.support_count,
                path_roles: item.roles.names(),
                path_overrides: item.overrides.names(),
                path_factor: item.roles.factor(item.overrides),
                static_score: item.static_score,
                file_count_at_selection: *count,
                diversity_factor: factor,
                score: item.hit.score,
                rank: item.hit.rank,
            });
        }
        *count = count.saturating_add(1);
        output.push(item.hit);
    }
    output
}

#[cfg(test)]
mod replay;
#[cfg(test)]
mod tests;
