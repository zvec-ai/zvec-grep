//! Adapts model lifecycle events to indexing progress at the operation boundary.

use crate::{
    api::index::progress::{
        IndexEmbeddingProgress, IndexEmbeddingStage, IndexProgress, IndexProgressPhase,
        IndexProgressReporter,
    },
    domain::model::ModelProgress,
    models::ModelProgressReporter,
};

pub(super) fn for_index(reporter: IndexProgressReporter) -> ModelProgressReporter {
    ModelProgressReporter::new(move |progress, concurrency| {
        reporter.report(index_progress(progress, Some(concurrency)));
    })
}

pub(super) fn for_extraction(
    reporter: IndexProgressReporter,
) -> std::sync::Arc<dyn Fn(ModelProgress) + Send + Sync> {
    std::sync::Arc::new(move |progress| {
        let resume = matches!(
            &progress,
            ModelProgress::Ready { .. } | ModelProgress::Skipped { .. }
        );
        reporter.report(index_progress(progress, None));
        if resume {
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: None,
                files_indexed: None,
                files_failed: None,
                detail: None,
                embedding: None,
            });
        }
    })
}

fn index_progress(progress: ModelProgress, concurrency: Option<usize>) -> IndexProgress {
    let (stage, model, downloaded_bytes, total_bytes, message) = match progress {
        ModelProgress::Preparing { model_ref: model } => {
            (IndexEmbeddingStage::Preparing, model, None, None, None)
        }
        ModelProgress::Downloading {
            model_ref: model,
            downloaded_bytes,
            total_bytes,
        } => (
            IndexEmbeddingStage::Downloading,
            model,
            downloaded_bytes,
            total_bytes,
            None,
        ),
        ModelProgress::Warning {
            model_ref: model,
            message,
        } => (
            IndexEmbeddingStage::Warning,
            model,
            None,
            None,
            Some(message),
        ),
        ModelProgress::Skipped {
            model_ref: model,
            message,
        } => (
            IndexEmbeddingStage::Skipped,
            model,
            None,
            None,
            Some(message),
        ),
        ModelProgress::Ready { model_ref: model } => {
            (IndexEmbeddingStage::Ready, model, None, None, None)
        }
    };
    IndexProgress {
        phase: IndexProgressPhase::Indexing,
        files_total: None,
        files_indexed: None,
        files_failed: None,
        detail: Some(format!("downloading {model}")),
        embedding: Some(IndexEmbeddingProgress {
            concurrency,
            max_concurrency: concurrency,
            retryable_failures: None,
            stage: Some(stage),
            model: Some(model),
            downloaded_bytes,
            total_bytes,
            message,
            selection: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_forwards_structured_download_and_ready_without_embedding_concurrency() {
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = observed.clone();
        let report = for_extraction(IndexProgressReporter::new(move |event| {
            captured.lock().expect("valid test fixture").push(event);
        }));
        report(ModelProgress::Downloading {
            model_ref: "layout/pp-s".into(),
            downloaded_bytes: Some(40),
            total_bytes: Some(100),
        });
        report(ModelProgress::Ready {
            model_ref: "layout/pp-s".into(),
        });
        let events = observed.lock().expect("valid test fixture");
        let download = events[0].embedding.as_ref().expect("valid test fixture");
        assert_eq!(download.stage, Some(IndexEmbeddingStage::Downloading));
        assert_eq!(
            (download.downloaded_bytes, download.total_bytes),
            (Some(40), Some(100))
        );
        assert_eq!(download.concurrency, None);
        assert_eq!(
            events[1]
                .embedding
                .as_ref()
                .expect("valid test fixture")
                .stage,
            Some(IndexEmbeddingStage::Ready)
        );
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].phase, IndexProgressPhase::Indexing);
        assert!(events[2].embedding.is_none());
        let restored = serde_json::from_value::<IndexProgress>(
            serde_json::to_value(&events[0]).expect("valid test fixture"),
        )
        .expect("valid test fixture");
        assert_eq!(restored, events[0]);
    }

    #[test]
    fn fallback_releases_download_priority_and_preserves_warning() {
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = observed.clone();
        let reporter = IndexProgressReporter::new(move |event| {
            captured.lock().expect("events").push(event);
        })
        .prioritize_model_progress();
        let report = for_extraction(reporter.clone());
        report(ModelProgress::Preparing {
            model_ref: "layout/pp-s".into(),
        });
        report(ModelProgress::Skipped {
            model_ref: "layout/pp-s".into(),
            message: "native text fallback".into(),
        });
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(2),
            files_indexed: Some(1),
            files_failed: Some(0),
            detail: None,
            embedding: None,
        });
        let events = observed.lock().expect("events");
        assert!(events.iter().any(|event| {
            event.embedding.as_ref().is_some_and(|model| {
                model.stage == Some(IndexEmbeddingStage::Skipped)
                    && model.message.as_deref() == Some("native text fallback")
            })
        }));
        assert_eq!(
            events.last().expect("indexing resumed").files_indexed,
            Some(1)
        );
    }

    #[test]
    fn preserves_model_lifecycle_details_and_effective_concurrency() {
        let model = "local/fixture";
        let events = [
            ModelProgress::Preparing {
                model_ref: model.into(),
            },
            ModelProgress::Downloading {
                model_ref: model.into(),
                downloaded_bytes: Some(4),
                total_bytes: Some(8),
            },
            ModelProgress::Warning {
                model_ref: model.into(),
                message: "fixture warning".into(),
            },
            ModelProgress::Ready {
                model_ref: model.into(),
            },
        ]
        .map(|event| index_progress(event, Some(3)));
        let stages = [
            IndexEmbeddingStage::Preparing,
            IndexEmbeddingStage::Downloading,
            IndexEmbeddingStage::Warning,
            IndexEmbeddingStage::Ready,
        ];
        for (event, stage) in events.iter().zip(stages) {
            assert_eq!(event.phase, IndexProgressPhase::Indexing);
            assert_eq!(event.detail.as_deref(), Some("downloading local/fixture"));
            assert_eq!(
                (event.files_total, event.files_indexed, event.files_failed),
                (None, None, None)
            );
            let embedding = event.embedding.as_ref().expect("model event");
            assert_eq!(embedding.stage, Some(stage));
            assert_eq!(embedding.model.as_deref(), Some(model));
            assert_eq!(embedding.concurrency, Some(3));
            assert_eq!(embedding.max_concurrency, Some(3));
            assert_eq!(embedding.retryable_failures, None);
        }
        let download = events[1].embedding.as_ref().expect("download event");
        assert_eq!(download.downloaded_bytes, Some(4));
        assert_eq!(download.total_bytes, Some(8));
        let warning = events[2].embedding.as_ref().expect("warning event");
        assert_eq!(warning.message.as_deref(), Some("fixture warning"));
        assert_eq!(warning.downloaded_bytes, None);
        assert_eq!(warning.total_bytes, None);
    }
}
