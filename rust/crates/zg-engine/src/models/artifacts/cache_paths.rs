//! Shared backend cache defaults for preparation and read-only inspection.

use std::{ffi::OsString, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalModelBackend {
    LlamaCpp,
    Model2Vec,
    Transformers,
}

pub(crate) fn default_model_cache(backend: LocalModelBackend) -> PathBuf {
    default_model_cache_from_env(backend, cfg!(windows), |key| std::env::var_os(key))
}

pub(super) fn default_model_cache_from_env(
    backend: LocalModelBackend,
    windows: bool,
    variable: impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    variable("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let home = if windows {
                variable("USERPROFILE").or_else(|| match backend {
                    LocalModelBackend::LlamaCpp => variable("HOME"),
                    LocalModelBackend::Model2Vec => {
                        let mut home = variable("HOMEDRIVE")?;
                        home.push(variable("HOMEPATH")?);
                        Some(home)
                    }
                    LocalModelBackend::Transformers => None,
                })
            } else {
                variable("HOME")
            };
            home.map(PathBuf::from).map(|home| {
                if windows && backend == LocalModelBackend::Transformers {
                    // Preserve the existing Windows Transformers download location.
                    home
                } else {
                    home.join(".zvec-grep")
                }
            })
        })
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BACKENDS: [LocalModelBackend; 3] = [
        LocalModelBackend::LlamaCpp,
        LocalModelBackend::Model2Vec,
        LocalModelBackend::Transformers,
    ];

    #[test]
    fn platform_defaults_and_runtime_overrides_preserve_backend_locations() {
        for backend in BACKENDS {
            for windows in [false, true] {
                let default = default_model_cache_from_env(backend, windows, |key| {
                    matches!(key, "HOME" | "USERPROFILE").then(|| OsString::from("/profile"))
                });
                assert_eq!(
                    default,
                    if windows && backend == LocalModelBackend::Transformers {
                        PathBuf::from("/profile/models")
                    } else {
                        PathBuf::from("/profile/.zvec-grep/models")
                    }
                );
                let overridden = default_model_cache_from_env(backend, windows, |key| {
                    (key == "ZVEC_GREP_HOME").then(|| OsString::from("/runtime"))
                });
                assert_eq!(overridden, PathBuf::from("/runtime/models"));
                assert_eq!(
                    default_model_cache_from_env(backend, windows, |_| None),
                    PathBuf::from(".zvec-grep/models")
                );
            }
        }
    }

    #[test]
    fn windows_fallbacks_match_existing_backend_preparation() {
        let llama = default_model_cache_from_env(LocalModelBackend::LlamaCpp, true, |key| {
            (key == "HOME").then(|| OsString::from("/fallback"))
        });
        assert_eq!(llama, PathBuf::from("/fallback/.zvec-grep/models"));
        let model2vec =
            default_model_cache_from_env(LocalModelBackend::Model2Vec, true, |key| match key {
                "HOMEDRIVE" => Some(OsString::from("D:")),
                "HOMEPATH" => Some(OsString::from("/Users/test")),
                _ => None,
            });
        assert_eq!(model2vec, PathBuf::from("D:/Users/test/.zvec-grep/models"));
    }
}
