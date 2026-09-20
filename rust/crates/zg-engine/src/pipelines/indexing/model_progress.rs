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
        reporter.report(index_progress(progress, concurrency));
    })
}

fn index_progress(progress: ModelProgress, concurrency: usize) -> IndexProgress {
    let (stage, model, downloaded_bytes, total_bytes, message) = match progress {
        ModelProgress::Preparing { model } => {
            (IndexEmbeddingStage::Preparing, model, None, None, None)
        }
        ModelProgress::Downloading {
            model,
            downloaded_bytes,
            total_bytes,
        } => (
            IndexEmbeddingStage::Downloading,
            model,
            downloaded_bytes,
            total_bytes,
            None,
        ),
        ModelProgress::Warning { model, message } => (
            IndexEmbeddingStage::Warning,
            model,
            None,
            None,
            Some(message),
        ),
        ModelProgress::Ready { model } => (IndexEmbeddingStage::Ready, model, None, None, None),
    };
    IndexProgress {
        phase: IndexProgressPhase::Indexing,
        files_total: None,
        files_indexed: None,
        files_failed: None,
        detail: Some(format!("downloading {model}")),
        embedding: Some(IndexEmbeddingProgress {
            concurrency: Some(concurrency),
            max_concurrency: Some(concurrency),
            retryable_failures: None,
            stage: Some(stage),
            model: Some(model),
            downloaded_bytes,
            total_bytes,
            message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_model_lifecycle_details_and_effective_concurrency() {
        let model = "local/fixture";
        let events = [
            ModelProgress::Preparing {
                model: model.into(),
            },
            ModelProgress::Downloading {
                model: model.into(),
                downloaded_bytes: Some(4),
                total_bytes: Some(8),
            },
            ModelProgress::Warning {
                model: model.into(),
                message: "fixture warning".into(),
            },
            ModelProgress::Ready {
                model: model.into(),
            },
        ]
        .map(|event| index_progress(event, 3));
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
