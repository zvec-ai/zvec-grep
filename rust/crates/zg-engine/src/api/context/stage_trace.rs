//! Opt-in retrieval observations carried to the calling transport.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::{Range, TextRange};

/// A single query group's stage observations. Disabled by default and omitted
/// from ordinary results; transports decide how to deliver the diagnostic data.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StageTrace {
    pub(crate) version: u32,
    pub(crate) group_id: String,
    pub(crate) limit: usize,
    pub(crate) watched_paths: Vec<String>,
    pub(crate) index_observed: bool,
    pub(crate) index_read_ms: f64,
    pub(crate) routes: BTreeMap<String, StageRecall>,
    pub(crate) sizes: StagePoolSizes,
    pub(crate) files: BTreeMap<String, BTreeMap<String, ObservedEntity>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct CandidateStages {
    pub(crate) recall: BTreeMap<String, usize>,
    pub(crate) merged: bool,
    pub(crate) rrf_rank: Option<usize>,
    pub(crate) loaded_rrf_rank: Option<usize>,
    pub(crate) loaded_range: Option<StageRange>,
    pub(crate) static_rank: Option<usize>,
    pub(crate) final_rank: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ObservedEntity {
    pub(crate) range: Option<StageRange>,
    pub(crate) searchable: bool,
    #[serde(flatten)]
    pub(crate) stages: CandidateStages,
}

impl ObservedEntity {
    pub(crate) fn new(range: Option<Range>, searchable: bool) -> Self {
        Self {
            range: range.map(Into::into),
            searchable,
            stages: CandidateStages::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageRange {
    Full,
    Byte {
        start_offset: u64,
        end_offset: u64,
    },
    Text(TextRange),
    Pdf {
        pages: Vec<u32>,
        start_byte_offset: usize,
        end_byte_offset: usize,
    },
}

impl From<Range> for StageRange {
    fn from(range: Range) -> Self {
        match range {
            Range::Full => Self::Full,
            Range::Byte(range) => Self::Byte {
                start_offset: range.start_offset(),
                end_offset: range.end_offset(),
            },
            Range::Text(range) => Self::Text(range),
            Range::Pdf(source) => Self::Pdf {
                pages: source.pages(0..source.text_len),
                start_byte_offset: source.markdown_start,
                end_byte_offset: source.markdown_start + source.text_len,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct RecallAttempt {
    pub(crate) depth: usize,
    pub(crate) returned_fragments: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct StageRecall {
    pub(crate) attempts: Vec<RecallAttempt>,
    pub(crate) unique_fragments: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct StagePoolSizes {
    pub(crate) merged: usize,
    pub(crate) fused: usize,
    pub(crate) loaded: usize,
    pub(crate) selected: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PdfBlock, PdfSourceMap, PdfSourcePart};

    #[test]
    fn pdf_trace_preserves_pages_and_document_offsets_without_block_payloads() {
        let source = PdfSourceMap {
            markdown_start: 100,
            text_len: 3,
            parts: [2, 2, 4]
                .into_iter()
                .enumerate()
                .map(|(offset, page)| PdfSourcePart {
                    range: offset..offset + 1,
                    block: PdfBlock {
                        page,
                        id: u32::try_from(offset + 1).expect("small block id"),
                        page_width: 612.0,
                        page_height: 792.0,
                        bbox: None,
                    },
                })
                .collect(),
        };
        source.validate("abc").expect("valid source map");
        let range = StageRange::from(Range::Pdf(std::sync::Arc::new(source)));
        let encoded = serde_json::to_value(&range).expect("serialize PDF trace");
        assert_eq!(
            encoded,
            serde_json::json!({
                "pdf": {"pages": [2, 4], "start_byte_offset": 100, "end_byte_offset": 103}
            })
        );
        assert_eq!(
            serde_json::from_value::<StageRange>(encoded).expect("deserialize PDF trace"),
            range
        );
    }
}
