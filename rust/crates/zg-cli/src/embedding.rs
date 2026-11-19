use zg_engine::api::index::embedding::{
    EmbeddingAnalysis, EmbeddingScene, EmbeddingSelection, EmbeddingSelectionSource,
};

fn workspace_line(analysis: &EmbeddingAnalysis) -> String {
    let scene = match analysis.scene {
        EmbeddingScene::Code => "code-dominant",
        EmbeddingScene::Text => "text-dominant",
        EmbeddingScene::Mixed => "mixed",
        EmbeddingScene::Unknown => "undetermined",
    };
    let total = u128::from(analysis.code.weight_bytes) + u128::from(analysis.text.weight_bytes);
    let evidence = (u128::from(analysis.code.weight_bytes) * 100 + total / 2)
        .checked_div(total)
        .map_or_else(
            || "no code/text evidence; using the fallback recommendation".into(),
            |code| format!("code {code}%, text {}%", 100 - code),
        );
    format!("Workspace: {scene} ({evidence})")
}

pub(crate) fn analysis_lines(analysis: &EmbeddingAnalysis) -> Vec<String> {
    vec![
        workspace_line(analysis),
        format!(
            "Recommended: {}",
            crate::theme::clean(&analysis.recommended_model)
        ),
    ]
}

pub(crate) fn selection_lines(selection: &EmbeddingSelection) -> Vec<String> {
    if selection.source == EmbeddingSelectionSource::Automatic
        && let Some(analysis) = &selection.analysis
        && analysis.recommended_model == selection.model
    {
        return vec![
            workspace_line(analysis),
            format!(
                "Using recommended {}",
                crate::theme::clean(&selection.model)
            ),
        ];
    }
    let mut lines = selection
        .analysis
        .as_ref()
        .map_or_else(Vec::new, analysis_lines);
    let source = match selection.source {
        EmbeddingSelectionSource::Explicit => "explicit selection",
        EmbeddingSelectionSource::ExistingIndex => "existing index",
        EmbeddingSelectionSource::Environment => "ZVEC_GREP_EMBEDDING",
        EmbeddingSelectionSource::Configuration => "global configuration",
        EmbeddingSelectionSource::Automatic => "automatic",
    };
    lines.push(format!(
        "Using: {} ({source})",
        crate::theme::clean(&selection.model)
    ));
    lines
}

#[derive(Default)]
pub(crate) struct SelectionDisplay {
    shown: Option<EmbeddingSelection>,
}

impl SelectionDisplay {
    pub(crate) fn lines(
        &mut self,
        progress: &zg_engine::api::index::progress::IndexProgress,
    ) -> Vec<String> {
        let Some(selection) = progress
            .embedding
            .as_ref()
            .and_then(|value| value.selection.as_ref())
        else {
            return Vec::new();
        };
        if self.shown.as_ref() == Some(selection) {
            return Vec::new();
        }
        self.shown = Some(selection.clone());
        selection_lines(selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::api::index::embedding::EmbeddingEvidence;

    #[test]
    fn automatic_selection_combines_recommendation_and_choice() {
        let mut selection = EmbeddingSelection {
            model: "local/bekko-256-int8".into(),
            source: EmbeddingSelectionSource::Automatic,
            analysis: Some(EmbeddingAnalysis {
                scene: EmbeddingScene::Text,
                code: EmbeddingEvidence::default(),
                text: EmbeddingEvidence {
                    files: 1,
                    weight_bytes: 100,
                },
                other_files: 0,
                recommended_model: "local/bekko-256-int8".into(),
            }),
        };
        assert_eq!(
            selection_lines(&selection),
            [
                "Workspace: text-dominant (code 0%, text 100%)",
                "Using recommended local/bekko-256-int8",
            ]
        );
        let analysis = selection.analysis.as_ref().expect("analysis");
        assert_eq!(
            analysis_lines(analysis)[1],
            "Recommended: local/bekko-256-int8"
        );
        selection.source = EmbeddingSelectionSource::Environment;
        selection.model = "local/potion-code-16m-v2".into();
        let lines = selection_lines(&selection);
        assert_eq!(lines[1], "Recommended: local/bekko-256-int8");
        assert_eq!(
            lines[2],
            "Using: local/potion-code-16m-v2 (ZVEC_GREP_EMBEDDING)"
        );
    }
}
