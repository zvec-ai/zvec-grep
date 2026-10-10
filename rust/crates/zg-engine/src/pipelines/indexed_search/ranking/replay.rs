//! Opt-in replay of local candidate snapshots through the production scorer.
use super::{tests::hit, *};
use crate::domain::{ByteRange, CodeMetadata, Range, SymbolType};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    repo: String,
    query: String,
    hits: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    path: String,
    start: u64,
    end: u64,
    symbol: Option<String>,
    scope: Option<String>,
    kind: Option<String>,
    score: f64,
    rank: usize,
}

#[test]
#[ignore = "requires local fixed candidate snapshots"]
fn replay_rule_ablation() {
    let directory =
        std::path::PathBuf::from(std::env::var_os("ZG_RANK_REPLAY").expect("snapshot directory"));
    let input = std::fs::read(directory.join("pools.json")).expect("read pools");
    let cases: Vec<Case> = serde_json::from_slice(&input).expect("decode pools");
    let variants = [
        ("baseline", Policy::BASELINE),
        (
            "path",
            Policy {
                path_weight: 0.35,
                ..Policy::BASELINE
            },
        ),
        (
            "weak",
            Policy {
                weak_names: true,
                ..Policy::BASELINE
            },
        ),
        (
            "intent",
            Policy {
                definition_intent: true,
                bonus_cap: 1.25,
                ..Policy::BASELINE
            },
        ),
        (
            "support",
            Policy {
                support_weight: 0.15,
                ..Policy::BASELINE
            },
        ),
        ("all", Policy::CURRENT),
        (
            "no_weak",
            Policy {
                weak_names: false,
                ..Policy::CURRENT
            },
        ),
        (
            "no_path",
            Policy {
                path_weight: 0.0,
                ..Policy::CURRENT
            },
        ),
        (
            "no_intent",
            Policy {
                definition_intent: false,
                ..Policy::CURRENT
            },
        ),
        (
            "path_weak_intent",
            Policy {
                support_weight: 0.0,
                ..Policy::CURRENT
            },
        ),
    ];
    let mut output = Vec::new();
    for case in cases {
        let candidates = snapshot_candidates(&case);
        let routes = [ResolvedSearchRoute {
            id: "fts".into(),
            mode: crate::api::context::options::ContextRouteMode::Fts,
            query: case.query.clone(),
        }];
        for (name, policy) in variants {
            let hits = rank_with_policy(
                candidates.clone(),
                &routes,
                &QueryFilter::default(),
                2.0 / 61.0,
                10,
                policy,
            );
            let hits = hits.iter().map(|h| {
                let original = &case.hits[h.entity.id.as_str().parse::<usize>().expect("candidate index")];
                serde_json::json!({"path": original.path, "start": original.start, "end": original.end, "trace": h.trace})
            }).collect::<Vec<_>>();
            output.push(serde_json::json!({"repo": case.repo, "query": case.query, "variant": name, "hits": hits}));
        }
    }
    std::fs::write(
        directory.join("replay.json"),
        serde_json::to_vec(&output).expect("encode replay"),
    )
    .expect("write replay");
}

fn snapshot_candidates(case: &Case) -> Vec<SearchHit> {
    let mut file_ids = HashMap::new();
    case.hits
        .iter()
        .enumerate()
        .map(|(i, candidate)| {
            let next = u32::try_from(file_ids.len() + 1).expect("file id");
            let file_id = *file_ids.entry(candidate.path.as_str()).or_insert(next);
            let kind = candidate.kind.as_deref().and_then(|kind| match kind {
                "class" => Some(SymbolType::Class),
                "function" => Some(SymbolType::Function),
                "interface" => Some(SymbolType::Interface),
                "alias" => Some(SymbolType::Alias),
                "enum" => Some(SymbolType::Enum),
                "module" => Some(SymbolType::Module),
                "value" => Some(SymbolType::Value),
                _ => None,
            });
            let metadata = candidate.symbol.as_ref().map(|name| CodeMetadata {
                symbol_type: kind,
                symbol_name: Some(name.clone()),
                scope: candidate.scope.clone(),
                signature: None,
                documentation: None,
            });
            let mut result = hit(
                &i.to_string(),
                file_id,
                &candidate.path,
                candidate.score,
                candidate.rank,
                metadata,
            );
            // Line intervals are conservative for same-line overlap in CLI snapshots.
            result.entity.source_range = Range::Byte(
                ByteRange::new(candidate.start, candidate.end + 1).expect("snapshot range"),
            );
            result
        })
        .collect::<Vec<_>>()
}
