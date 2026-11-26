//! Opt-in retrieval observations carried to the calling transport.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::{ByteRange, Range, TextRange};

/// A single query group's stage observations. Disabled by default and omitted
/// from ordinary results; transports decide how to deliver the diagnostic data.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StageTrace {
    /// Kind tables searched by this query group and score space.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<crate::api::context::options::ContentKind>,
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
    pub(crate) fn new(range: Option<StageRange>, searchable: bool) -> Self {
        Self {
            range,
            searchable,
            stages: CandidateStages::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageRange {
    Full,
    Byte(ByteRange),
    Text(TextRange),
    Pdf {
        pages: Vec<u32>,
        start_byte_offset: usize,
        end_byte_offset: usize,
    },
}

impl StageRange {
    pub(crate) fn from_location(range: Range, offsets: Option<(u64, u64)>) -> Self {
        match range {
            Range::Full => Self::Full,
            Range::Byte(range) => Self::Byte(range),
            Range::Text(range) => Self::Text(range),
            Range::Pdf(location) => {
                let (start, end) = offsets.unwrap_or_default();
                Self::Pdf {
                    pages: location.page_numbers(),
                    start_byte_offset: usize::try_from(start)
                        .expect("offset originates from in-memory text"),
                    end_byte_offset: usize::try_from(end)
                        .expect("offset originates from in-memory text"),
                }
            }
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
    use serde_json::json;

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
        let range = StageRange::from_location(
            Range::Pdf(source.locate(0..source.text_len).expect("location").into()),
            Some((100, 103)),
        );
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

    #[test]
    fn trace_ranges_preserve_json_and_reject_reversed_bytes() {
        let mut trace = serde_json::to_value(ObservedEntity::new(None, true)).expect("trace JSON");
        for range in [
            json!("full"),
            json!({"byte": {"start_offset": 3, "end_offset": 9}}),
            json!({"byte": {"start_offset": 3, "end_offset": 3}}),
            json!({"text": {
                "start": {"byte_offset": 3, "line": 1, "byte_column": 3},
                "end": {"byte_offset": 9, "line": 1, "byte_column": 9}
            }}),
        ] {
            trace["range"] = range.clone();
            trace["loaded_range"] = range;
            let observed: ObservedEntity = serde_json::from_value(trace.clone()).expect("decode");
            assert_eq!(serde_json::to_value(observed).expect("encode"), trace);
        }
        for field in ["range", "loaded_range"] {
            let mut invalid = trace.clone();
            invalid[field] = json!({"byte": {"start_offset": 9, "end_offset": 3}});
            assert!(
                serde_json::from_value::<ObservedEntity>(invalid).is_err(),
                "{field} accepted reversed byte offsets"
            );
        }
    }
}
