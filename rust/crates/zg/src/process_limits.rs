//! Process policy for the CLI and its daemon, before starting runtime threads.

#[cfg(unix)]
pub(super) fn configure_file_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    // Native collections keep several index files open. macOS shells commonly
    // start with a soft limit of 256, which is insufficient for a multimodal index.
    const RECOMMENDED_OPEN_FILES: u64 = 4096;
    let limit = getrlimit(Resource::Nofile);
    let Some(current) = limit.current else {
        return;
    };
    let target = limit.maximum.map_or(RECOMMENDED_OPEN_FILES, |maximum| {
        maximum.min(RECOMMENDED_OPEN_FILES)
    });
    if target < RECOMMENDED_OPEN_FILES {
        eprintln!(
            "Warning: the hard open-file limit is {target}; indexing may require at least \
             {RECOMMENDED_OPEN_FILES}. Raise the shell's open-file limit if permitted."
        );
    }
    if current >= target {
        return;
    }
    if let Err(error) = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(target),
            maximum: limit.maximum,
        },
    ) {
        eprintln!(
            "Warning: cannot raise the soft open-file limit from {current} to {target}: \
             {error}. Indexing may fail with too many open files; check the shell's open-file limit."
        );
    }
}

#[cfg(not(unix))]
pub(super) fn configure_file_limit() {}
