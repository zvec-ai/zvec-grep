use std::{collections::HashMap, env};

use super::{catalog::get_embedding_model_catalog_entry, spi::ModelError};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolveEmbeddingReferenceOptions {
    pub explicit: Option<String>,
    pub existing: Option<String>,
    pub global_default: Option<String>,
    pub environment: Option<HashMap<String, String>>,
    pub fallback: Option<String>,
}

/// Resolves an embedding reference with the same precedence as TypeScript.
///
/// # Errors
///
/// Returns an error only when `ZVEC_GREP_EMBEDDING` selects a model outside
/// the catalog. Explicit, existing, global and fallback values are not
/// validated by this function.
pub fn resolve_embedding_reference(
    options: ResolveEmbeddingReferenceOptions,
) -> Result<Option<String>, ModelError> {
    if options.explicit.is_some() {
        return Ok(options.explicit);
    }
    if options.existing.is_some() {
        return Ok(options.existing);
    }

    let environment_reference = options
        .environment
        .as_ref()
        .and_then(|environment| environment.get("ZVEC_GREP_EMBEDDING").cloned())
        .or_else(|| {
            if options.environment.is_none() {
                env::var("ZVEC_GREP_EMBEDDING").ok()
            } else {
                None
            }
        })
        .and_then(|value| {
            let normalized = value.trim();
            (!normalized.is_empty()).then(|| normalized.to_owned())
        });

    if let Some(reference) = &environment_reference
        && get_embedding_model_catalog_entry(reference).is_none()
    {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            format!(
                "Invalid ZVEC_GREP_EMBEDDING: unsupported model {reference}. Run `zg help models` to list supported models."
            ),
            Some("source=ZVEC_GREP_EMBEDDING".to_owned()),
        ));
    }

    Ok(environment_reference
        .or(options.global_default)
        .or(options.fallback))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};

    #[test]
    fn resolver_matches_typescript_precedence_and_validation() {
        let environment = HashMap::from([(
            "ZVEC_GREP_EMBEDDING".to_owned(),
            "local/embeddinggemma-300m".to_owned(),
        )]);
        let resolved = resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
            explicit: Some("qwen/qwen3.7-text-embedding".to_owned()),
            existing: Some("local/qwen3-embedding-0.6b".to_owned()),
            global_default: Some("local/potion-code-16m-v2".to_owned()),
            environment: Some(environment),
            fallback: None,
        })
        .expect("explicit model should resolve");
        assert_eq!(resolved.as_deref(), Some("qwen/qwen3.7-text-embedding"));

        let blank_environment =
            HashMap::from([("ZVEC_GREP_EMBEDDING".to_owned(), "   ".to_owned())]);
        let resolved = resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
            global_default: Some("local/potion-code-16m-v2".to_owned()),
            environment: Some(blank_environment),
            ..ResolveEmbeddingReferenceOptions::default()
        })
        .expect("blank environment should fall through");
        assert_eq!(resolved.as_deref(), Some("local/potion-code-16m-v2"));

        let invalid_environment =
            HashMap::from([("ZVEC_GREP_EMBEDDING".to_owned(), "unknown/model".to_owned())]);
        let error = resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
            environment: Some(invalid_environment),
            ..ResolveEmbeddingReferenceOptions::default()
        })
        .expect_err("unsupported environment model must fail");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert!(error.to_string().contains("zg help models"));
    }
}
