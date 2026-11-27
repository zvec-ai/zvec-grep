use super::*;
use crate::models::LayoutKind;
use liteparse::types::TextItem;
use tokio_util::sync::CancellationToken;

fn page() -> RenderedPage {
    let mut page: liteparse::types::Page = serde_json::from_value(serde_json::json!({
        "page_number": 1, "page_width": 100.0, "page_height": 100.0, "text_items": []
    }))
    .expect("valid test fixture");
    page.text_items = vec![
        TextItem {
            text: "figure noise".into(),
            x: 10.0,
            y: 10.0,
            width: 20.0,
            height: 10.0,
            ..TextItem::default()
        },
        TextItem {
            text: "Figure 1: caption".into(),
            x: 10.0,
            y: 80.0,
            width: 40.0,
            height: 10.0,
            ..TextItem::default()
        },
    ];
    RenderedPage {
        page,
        tables: vec![],
        image: Ok(Some(LayoutImage {
            width: 100,
            height: 100,
            rgb: vec![255; 30_000].into(),
        })),
    }
}

fn region() -> LayoutRegion {
    LayoutRegion {
        kind: LayoutKind::Image,
        confidence: 0.9,
        bounds: [0.0, 0.0, 50.0, 50.0],
    }
}

#[tokio::test]
async fn bad_page_preserves_all_native_text_and_next_page_still_uses_layout() {
    let disabled = AtomicBool::new(false);
    let path = SourcePath::new("fixture.pdf").expect("valid test fixture");
    let mut enhancement = Enhancement {
        disabled: &disabled,
        path: &path,
        options: LayoutOptions::default(),
        warnings: vec![],
    };
    for predicted in [
        Err(ModelError::internal("invalid page prediction")),
        Ok(vec![
            region(),
            LayoutRegion {
                bounds: [f32::NAN; 4],
                ..region()
            },
        ]),
    ] {
        let mut bad = page();
        enhancement
            .apply(&mut bad, |_| async { predicted })
            .await
            .expect("valid test fixture");
        assert_eq!(bad.page.text_items.len(), 2);
        assert!(!disabled.load(Ordering::Relaxed));
    }
    let mut next = page();
    next.page.page_number = 2;
    enhancement
        .apply(&mut next, |_| async { Ok(vec![region()]) })
        .await
        .expect("valid test fixture");
    assert_eq!(next.page.text_items.len(), 1);
    assert_eq!(next.page.text_items[0].text, "Figure 1: caption");
    assert_eq!(enhancement.warnings.len(), 2);
    assert!(enhancement.warnings[0].contains("fixture.pdf: page 1"));
}

#[tokio::test]
async fn shared_failure_skips_further_inference_in_the_operation() {
    let disabled = AtomicBool::new(false);
    let path = SourcePath::new("fixture.pdf").expect("valid test fixture");
    let mut enhancement = Enhancement {
        disabled: &disabled,
        path: &path,
        options: LayoutOptions::default(),
        warnings: vec![],
    };
    let mut first = page();
    enhancement
        .apply(&mut first, |_| async {
            Err(ModelError::internal("model unavailable").shared())
        })
        .await
        .expect("valid test fixture");
    assert_eq!(first.page.text_items.len(), 2);
    assert!(disabled.load(Ordering::Relaxed));
    let mut next = page();
    enhancement
        .apply(&mut next, |_| async {
            panic!("disabled model must not run")
        })
        .await
        .expect("valid test fixture");
    assert_eq!(next.page.text_items.len(), 2);
}

#[tokio::test]
async fn rendering_failure_falls_back_but_cancellation_never_does() {
    let disabled = AtomicBool::new(false);
    let path = SourcePath::new("fixture.pdf").expect("valid test fixture");
    let signal = CancellationToken::new();
    let mut enhancement = Enhancement {
        disabled: &disabled,
        path: &path,
        options: LayoutOptions {
            signal: Some(signal.clone()),
            ..LayoutOptions::default()
        },
        warnings: vec![],
    };
    let mut failed = page();
    failed.image = Err("bitmap allocation failed".into());
    enhancement
        .apply(&mut failed, |_| async { panic!("no image") })
        .await
        .expect("valid test fixture");
    assert_eq!(failed.page.text_items.len(), 2);
    assert!(!disabled.load(Ordering::Relaxed));
    let error = enhancement
        .apply(&mut page(), |_| async {
            Err(ModelError::cancelled("cancelled"))
        })
        .await
        .expect_err("expected failure");
    assert_eq!(error.code(), EngineError::CANCELLED);
    signal.cancel();
    let error = enhancement
        .apply(&mut page(), |_| async {
            panic!("cancelled before inference")
        })
        .await
        .expect_err("expected failure");
    assert_eq!(error.code(), EngineError::CANCELLED);
    assert_eq!(enhancement.warnings.len(), 1);
}
