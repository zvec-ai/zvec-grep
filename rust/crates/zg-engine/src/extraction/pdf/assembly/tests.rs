use super::*;

#[test]
fn pdf_background_does_not_swallow_text_but_chart_text_is_filtered() {
    let item = |s: &str, bounds| Item {
        text: s.into(),
        bounds,
        size: 10.0,
        bold: false,
    };
    let items = vec![
        item("Independent paragraph", [10.0, 10.0, 160.0, 20.0]),
        item("chart label", [20.0, 50.0, 60.0, 60.0]),
        item("Figure 1: Caption", [10.0, 80.0, 130.0, 90.0]),
    ];
    let regions = vec![
        Region {
            kind: LayoutKind::Image,
            bounds: [0.0, 0.0, 200.0, 200.0],
        },
        Region {
            kind: LayoutKind::Chart,
            bounds: [10.0, 40.0, 180.0, 70.0],
        },
        Region {
            kind: LayoutKind::FigureCaption,
            bounds: [10.0, 80.0, 140.0, 92.0],
        },
    ];
    let page = assemble_page(1, 200.0, 200.0, &items, regions);
    let output = finish(
        vec![page],
        &Bookmarks::new(),
        SourcePath::new("test.pdf").expect("valid test fixture"),
    )
    .expect("valid test fixture");
    assert!(output.text.text.contains("Independent paragraph"));
    assert!(output.text.text.contains("Figure 1: Caption"));
    assert!(!output.text.text.contains("chart label"));
}

#[test]
fn pdf_heading_and_cross_page_word_preserve_provenance() {
    let item = |s: &str, bounds, size, bold| Item {
        text: s.into(),
        bounds,
        size,
        bold,
    };
    let a = assemble_page(
        1,
        300.0,
        400.0,
        &[
            item("1 Overview", [20.0, 60.0, 180.0, 75.0], 14.0, true),
            item(
                "This paragraph continues with an incom-",
                [20.0, 310.0, 270.0, 320.0],
                10.0,
                false,
            ),
        ],
        vec![],
    );
    let b = assemble_page(
        2,
        300.0,
        400.0,
        &[
            item(
                "plete word on the next page.",
                [20.0, 30.0, 260.0, 40.0],
                10.0,
                false,
            ),
            item("1.1 Detail", [20.0, 100.0, 150.0, 113.0], 12.0, true),
        ],
        vec![],
    );
    let output = finish(
        vec![a, b],
        &Bookmarks::new(),
        SourcePath::new("test.pdf").expect("valid test fixture"),
    )
    .expect("valid test fixture");
    assert!(output.text.text.starts_with("# 1 Overview"));
    assert!(output.text.text.contains("incomplete word"));
    assert!(output.text.text.contains("## 1.1 Detail"));
    output
        .source_map
        .validate(&output.text.text)
        .expect("valid test fixture");
    let index = output
        .text
        .text
        .find("incomplete")
        .expect("valid test fixture");
    assert_eq!(output.source_map.pages(index..index + 10), [1, 2]);
}

#[test]
#[ignore = "Set ZG_PDF_REPLAY to the existing round13/replayed directory; reuses frozen native items and detections, no extraction or inference"]
fn pdf_frozen_assembly_replay() {
    let root = std::path::PathBuf::from(std::env::var_os("ZG_PDF_REPLAY").expect("ZG_PDF_REPLAY"));
    let oracle = std::path::PathBuf::from(
        std::env::var_os("ZG_PDF_ORACLE")
            .expect("ZG_PDF_ORACLE: frozen assembler + accepted image policy outputs"),
    );
    let mut mismatches = Vec::new();
    for entry in std::fs::read_dir(root).expect("valid test fixture") {
        let path = entry.expect("valid test fixture").path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let input: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("valid test fixture"))
                .expect("valid test fixture");
        let mut pages = Vec::new();
        let stem = path
            .file_stem()
            .expect("valid test fixture")
            .to_string_lossy();
        let bookmarks: Bookmarks = serde_json::from_slice(
            &std::fs::read(oracle.join(format!("{stem}.bookmarks.json")))
                .expect("valid test fixture"),
        )
        .expect("valid test fixture");
        for pg in input["pages"].as_array().expect("valid test fixture") {
            let number = u32::try_from(pg["page"].as_u64().expect("valid test fixture"))
                .expect("valid test fixture");
            let bbox = |v: &serde_json::Value| {
                std::array::from_fn(|i| v[i].as_f64().expect("valid test fixture"))
            };
            let items: Vec<_> = pg["items"]
                .as_array()
                .expect("valid test fixture")
                .iter()
                .map(|i| Item {
                    text: i["text"].as_str().expect("valid test fixture").into(),
                    bounds: bbox(&i["bbox"]),
                    size: i["font_size"].as_f64().expect("valid test fixture"),
                    bold: i["bold"].as_bool().expect("valid test fixture"),
                })
                .collect();
            let regions = pg["regions"]
                .as_array()
                .expect("valid test fixture")
                .iter()
                .map(|r| Region {
                    kind: label(r["label"].as_str().expect("valid test fixture")),
                    bounds: bbox(&r["bbox"]),
                })
                .collect();
            pages.push(assemble_page(
                number,
                pg["width"].as_f64().expect("valid test fixture"),
                pg["height"].as_f64().expect("valid test fixture"),
                &items,
                regions,
            ));
        }
        let debug_pages:Vec<_>=pages.iter().map(|pg|serde_json::json!({"page":pg.number,"blocks":pg.blocks.iter().map(|b|serde_json::json!({"text":b.text,"bbox":b.bounds,"first":b.first,"last":b.last,"size":b.size,"bold":b.bold,"kind":format!("{:?}",b.kind),"region":b.region})).collect::<Vec<_>>()})).collect();
        let output = finish(
            pages,
            &bookmarks,
            SourcePath::new("replay.pdf").expect("valid test fixture"),
        )
        .expect("valid test fixture");
        let expected =
            std::fs::read_to_string(oracle.join(format!("{stem}.md"))).expect("valid test fixture");
        if output.text.text != expected {
            let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/pdf-validation/replay");
            std::fs::create_dir_all(&directory).expect("valid test fixture");
            let stem = path
                .file_stem()
                .expect("valid test fixture")
                .to_string_lossy();
            std::fs::write(
                directory.join(format!("{stem}.pages.json")),
                serde_json::to_vec(&debug_pages).expect("valid test fixture"),
            )
            .expect("valid test fixture");
            std::fs::write(
                directory.join(format!("{stem}.actual.md")),
                output.text.text,
            )
            .expect("valid test fixture");
            std::fs::write(directory.join(format!("{stem}.expected.md")), expected)
                .expect("valid test fixture");
            mismatches.push(stem.to_string());
        }
    }
    assert!(
        mismatches.is_empty(),
        "Assembly differences: {mismatches:?}"
    );
}
fn label(s: &str) -> LayoutKind {
    match s {
        "paragraph_title" => LayoutKind::Heading,
        "doc_title" | "document_title" => LayoutKind::DocumentTitle,
        "image" | "figure" => LayoutKind::Image,
        "chart" => LayoutKind::Chart,
        "figure_title" => LayoutKind::FigureCaption,
        "table_title" => LayoutKind::TableCaption,
        "chart_title" => LayoutKind::ChartCaption,
        "table" => LayoutKind::Table,
        "formula" | "display_formula" => LayoutKind::Formula,
        "formula_number" => LayoutKind::FormulaNumber,
        "header" => LayoutKind::Header,
        "footer" => LayoutKind::Footer,
        "number" => LayoutKind::PageNumber,
        "footnote" => LayoutKind::Footnote,
        "header_image" => LayoutKind::HeaderImage,
        "footer_image" => LayoutKind::FooterImage,
        _ => LayoutKind::Text,
    }
}
