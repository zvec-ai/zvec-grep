//! Rerank recalled code within the positions assigned to code by fusion.
mod paths;
mod query;
mod support;

use std::{cmp::Ordering, collections::HashMap};

use super::{
    pipeline::{ResolvedSearchRoute, SearchHit},
    stage_trace::StageTraceCollector,
};
use crate::{
    api::context::{options::QueryFilter, result::SearchRankingTrace},
    domain::{ContentKind, EntityMetadata, FileId, SymbolType},
};
use paths::Roles;
use query::QueryFeatures;

const FILE_DECAY: f64 = 0.50;

/// Internal policy values also provide a fixed-pool ablation seam for tests.
#[derive(Clone, Copy)]
struct Policy {
    name: &'static str,
    path_weight: f64,
    weak_names: bool,
    definition_intent: bool,
    support_weight: f64,
    bonus_cap: f64,
    anchored_functions: bool,
}

impl Policy {
    const CURRENT: Self = Self {
        name: "code-rules",
        path_weight: 0.35,
        weak_names: true,
        definition_intent: true,
        support_weight: 0.15,
        bonus_cap: 1.25,
        anchored_functions: true,
    };

    #[cfg(test)]
    const BASELINE: Self = Self {
        name: "code-rules-baseline",
        path_weight: 0.0,
        weak_names: false,
        definition_intent: false,
        support_weight: 0.0,
        bonus_cap: 0.50,
        anchored_functions: false,
    };
}

struct Scored {
    hit: SearchHit,
    base: f64,
    static_score: f64,
    independent_anchor: bool,
    symbol_match: &'static str,
    symbol_bonus: f64,
    definition_bonus: f64,
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
    let query = QueryFeatures::new(&queries.join("\n"));
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
            let path = hit.file.relative_path().display().to_string();
            let (symbol_match, symbol_bonus, definition_bonus, independent_anchor) =
                match hit.entity.metadata() {
                    Some(EntityMetadata::Code(code)) => {
                        let (kind, symbol, definition) = query.symbol_bonus_with(code, policy);
                        let anchored = policy.anchored_functions
                            && code.symbol_type == Some(SymbolType::Function)
                            && code.symbol_name.as_ref().is_some_and(|s| !s.is_empty())
                            && support::interval(hit.entity.source_range()).is_some()
                            && query.names_function_file_or_scope(&path, code.scope.as_deref());
                        (kind, symbol, definition, anchored)
                    }
                    _ => ("none", 0.0, 0.0, false),
                };
            let base = hit.score / maximum_rrf;
            let mut path_bonus = policy.path_weight * query.path_affinity(&path);
            // Weak lexical coverage is one signal even when repeated in the path.
            if symbol_match == "tokens" {
                path_bonus = (path_bonus - symbol_bonus).max(0.0);
            }
            let static_score = (base
                + (symbol_bonus + definition_bonus + path_bonus).min(policy.bonus_cap))
                * roles.factor(overrides);
            Scored {
                hit,
                base,
                static_score,
                independent_anchor,
                symbol_match,
                symbol_bonus,
                definition_bonus,
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
    for item in &mut scored {
        item.hit.score = item.static_score;
    }
    for &rank in code_ranks {
        let best = scored
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| compare(a, b, a.hit.score, b.hit.score))
            .map_or(0, |(index, _)| index);
        let mut item = scored.swap_remove(best);
        let file = item.hit.file.id();
        let count = selected_per_file.entry(file).or_default();
        let factor = if single_file || item.independent_anchor {
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
                normalized_base: item.base,
                symbol_match: item.symbol_match.to_owned(),
                symbol_bonus: item.symbol_bonus,
                definition_bonus: item.definition_bonus,
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
        if !single_file && output.len() + 1 < code_ranks.len() {
            // Only this file's scores can change. Once a selected result covers a
            // function or shares its scoped name, the waiver stays disabled.
            let divisor = 1.0 + FILE_DECAY * f64::from(*count);
            for other in scored
                .iter_mut()
                .filter(|other| other.hit.file.id() == file)
            {
                if other.independent_anchor {
                    let same_symbol = matches!((other.hit.entity.metadata(), item.hit.entity.metadata()),
                        (Some(EntityMetadata::Code(a)), Some(EntityMetadata::Code(b)))
                        if a.scope == b.scope && a.symbol_name == b.symbol_name);
                    other.independent_anchor = !same_symbol
                        && !support::overlaps(
                            other.hit.entity.source_range(),
                            item.hit.entity.source_range(),
                        );
                }
                if !other.independent_anchor {
                    other.hit.score = other.static_score / divisor;
                }
            }
        }
        output.push(item.hit);
    }
    output
}

#[cfg(test)]
mod replay;
#[cfg(test)]
mod tests;
