//! Bounded support from independent definitions within a file.
use std::collections::{HashMap, HashSet};

use super::Scored;
use crate::domain::{EntityMetadata, FileId, Range, SymbolType};

pub(super) fn apply(scored: &mut [Scored], weight: f64) {
    let mut files = HashMap::<FileId, Vec<usize>>::new();
    for (index, item) in scored.iter().enumerate() {
        let Some(EntityMetadata::Code(code)) = &item.hit.entity.metadata else {
            continue;
        };
        if code
            .symbol_type
            .is_none_or(|kind| kind == SymbolType::Value)
            || code.symbol_name.is_none()
            || (item.symbol_bonus == 0.0 && item.path_bonus == 0.0)
            || interval(item.hit.entity.source_range).is_none()
        {
            continue;
        }
        files.entry(item.hit.file.id).or_default().push(index);
    }
    for mut indices in files.into_values() {
        indices.sort_by(|&a, &b| {
            scored[b]
                .static_score
                .total_cmp(&scored[a].static_score)
                .then_with(|| scored[a].hit.rank.cmp(&scored[b].hit.rank))
                .then_with(|| {
                    scored[a]
                        .hit
                        .entity
                        .id
                        .as_str()
                        .cmp(scored[b].hit.entity.id.as_str())
                })
        });
        let mut selected = Vec::<usize>::new();
        let mut names = HashSet::new();
        for index in indices {
            let Some(EntityMetadata::Code(code)) = &scored[index].hit.entity.metadata else {
                continue;
            };
            let name = (code.scope.as_deref(), code.symbol_name.as_deref());
            if names.contains(&name)
                || selected.iter().any(|&other| {
                    overlaps(
                        scored[index].hit.entity.source_range,
                        scored[other].hit.entity.source_range,
                    )
                })
            {
                continue;
            }
            names.insert(name);
            selected.push(index);
            if selected.len() == 3 {
                break;
            }
        }
        if selected.len() < 2 {
            continue;
        }
        let best = selected[0];
        let support = selected
            .iter()
            .skip(1)
            .map(|&i| scored[i].base.min(1.0))
            .sum::<f64>()
            / 2.0;
        let bonus = weight * support * scored[best].roles.factor(scored[best].overrides);
        scored[best].static_score += bonus;
        scored[best].support_bonus = bonus;
        scored[best].support_count = u32::try_from(selected.len() - 1).unwrap_or(2);
    }
}

fn interval(range: Range) -> Option<(u64, u64)> {
    match range {
        Range::Byte(range) => Some((range.start_offset(), range.end_offset())),
        Range::Text(range) => Some((
            u64::try_from(range.start_byte_offset()).ok()?,
            u64::try_from(range.end_byte_offset()).ok()?,
        )),
        Range::Full => None,
    }
    .filter(|(start, end)| start < end)
}

fn overlaps(a: Range, b: Range) -> bool {
    match (interval(a), interval(b)) {
        (Some((a0, a1)), Some((b0, b1))) => a0 < b1 && b0 < a1,
        _ => true,
    }
}
