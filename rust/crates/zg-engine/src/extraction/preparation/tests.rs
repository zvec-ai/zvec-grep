use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    calls: AtomicUsize,
}
#[async_trait]
impl ExtractionPort for Fixture {
    async fn prepare(
        &self,
        request: ExtractionRequest<'_>,
    ) -> Result<PreparedSource, ExtractionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match request.bytes.as_ref() {
            b"dependency" => Err(ExtractionError::Dependency(EngineError::internal(
                "missing runtime",
            ))),
            b"document" => Err(EngineError::invalid_argument("invalid source").into()),
            b"cancel" => Err(ExtractionError::Dependency(EngineError::cancelled(
                "cancelled",
            ))),
            _ => Ok(PreparedSource::Text(TextSource {
                relative_path: request.path.clone(),
                formats: vec![FileFormat::Markdown],
                text: "# Heading\n\nContent".into(),
            })),
        }
    }
}
fn request<'a>(
    path: &'a SourcePath,
    formats: &'a [FileFormat],
    bytes: &'static [u8],
) -> ExtractionRequest<'a> {
    ExtractionRequest {
        path,
        formats,
        bytes: bytes.into(),
        signal: CancellationToken::new(),
        on_progress: None,
    }
}

#[tokio::test]
async fn dependency_failure_is_scoped_to_source_kind_and_operation() {
    let fixture = Fixture {
        calls: AtomicUsize::new(0),
    };
    let path = SourcePath::new("document.pdf").expect("valid fixture");
    let operation = ExtractionOperation::new(&fixture);
    assert!(
        operation
            .prepare(request(&path, &[FileFormat::Pdf], b"dependency"))
            .await
            .is_err()
    );
    assert!(
        operation
            .prepare(request(&path, &[FileFormat::Pdf], b"ok"))
            .await
            .is_err()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        operation
            .prepare(request(&path, &[FileFormat::Markdown], b"ok"))
            .await
            .is_ok()
    );
    assert!(
        ExtractionOperation::new(&fixture)
            .prepare(request(&path, &[FileFormat::Pdf], b"ok"))
            .await
            .is_ok()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn document_failure_and_cancellation_do_not_poison_dependencies() {
    let fixture = Fixture {
        calls: AtomicUsize::new(0),
    };
    let path = SourcePath::new("document.pdf").expect("valid fixture");
    let operation = ExtractionOperation::new(&fixture);
    for input in [b"document".as_slice(), b"cancel"] {
        assert!(
            operation
                .prepare(request(&path, &[FileFormat::Pdf], input))
                .await
                .is_err()
        );
        assert!(
            operation
                .prepare(request(&path, &[FileFormat::Pdf], b"ok"))
                .await
                .is_ok()
        );
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    assert!(
        operation
            .prepare(request(&path, &[FileFormat::Pdf], b"dependency"))
            .await
            .is_err()
    );
    let cancelled = request(&path, &[FileFormat::Pdf], b"ok");
    cancelled.signal.cancel();
    let Err(error) = operation.prepare(cancelled).await else {
        panic!("must cancel")
    };
    assert_eq!(EngineError::from(error).code(), EngineError::CANCELLED);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn text_preparation_preserves_decoding_and_never_prepares_models() {
    let cache = tempfile::tempdir().expect("valid fixture");
    let models = ModelRuntimeManager::new();
    let extractor = ExtractionService::new(models.clone(), Some(cache.path().to_owned()));
    let path = SourcePath::new("notes.md").expect("valid fixture");
    let source = extractor
        .prepare(request(
            &path,
            &[FileFormat::Markdown],
            b"\xef\xbb\xbf# Heading\n\nContent",
        ))
        .await
        .expect("prepare native Markdown");
    assert_eq!(source.text(), "# Heading\n\nContent");
    assert!(
        !source
            .extract(ChunkOptions::default(), false)
            .expect("valid fixture")
            .fragments
            .is_empty()
    );
    assert_eq!(models.snapshot().cached_runtimes, 0);
    assert!(
        std::fs::read_dir(cache.path())
            .expect("valid fixture")
            .next()
            .is_none()
    );
}
