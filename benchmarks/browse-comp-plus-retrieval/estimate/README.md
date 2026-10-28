# BrowseComp index time scout

This estimates full-corpus indexing time from completed 256/1024/4096-document
indexes. It does **not** satisfy the full-corpus release comparison.

The dedicated workflow runs unchanged official Node 0.2.2 and the explicitly
pinned Rust source on the same Linux runner, sequentially, with local
`potion-multilingual-128m`, CPU, identical model hashes and embedding concurrency
four. Each measured index is fresh and includes CLI startup, model loading,
writes, final optimization and shutdown. Download warmups are separate.

`sample.py` preserves the locked raw UTF-8 corpus, divides documents into
equal-count length strata and picks one document per stratum. Among 128 fixed
seed offsets it chooses the sample with total bytes closest to its proportional
corpus share, before any performance observation. Manifests retain every path,
hash, byte count, seed offset, length quantile and aggregate error.

`measure.py` records external wall time, sampled process-tree RSS and Linux
`wait4` CPU/filesystem counters. Raw stdout/stderr and timestamped progress are
flushed during indexing. Public Rust debug timers are preserved, including
overlapping worker durations; Node's missing timers remain unavailable.
Public status verifies ready state, exact file coverage and actual stored entity
counts. Internal embedding inputs are not exposed by either public status.

Every completed phase checkpoints its result and appends a GitHub Summary.
Failures/timeouts remain failures and retain available evidence. Index and
consumer directories are excluded from uploaded artifacts.

Small-index byte-linear extrapolation does not establish large-index scaling.
The earlier Node attempt indexed only 56,981 of 100,195 documents in 5.000 hours
before timeout (8.792 hours by simple extrapolation, without finalization).
Full acceptance still needs both fresh 100,195-document indexes and the fixed
ten questions × three modes × five calls per product. GitHub hosted jobs have a
six-hour limit; the scout does not claim to solve the full-run execution window.
