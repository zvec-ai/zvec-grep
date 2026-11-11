# Benchmark working rules

These rules apply to `benchmarks/` and its subdirectories. Read the suite README before changing its execution or scoring behavior.

## Shared rules

1. **Make evaluations reproducible.** Document the evaluation type (Retrieval-only or E2E), purpose, data sources, pinned versions, and commands in the suite README.
2. **Define scoring before comparisons.** Specify metrics, denominators, repetitions, limits, failure handling, and outlier handling. State what changes between runs and what stays fixed.
3. **Support Gold with evidence.** Check each target against its question and retain an independent review. Keep reference answers, Gold, and reports outside searchable data and hidden from the answering agent.
4. **Keep original records.** Preserve traces, failures, retries, and missing-data records. Keep published results; save corrections as new versions.
5. **Make results traceable.** Distinguish rescoring saved responses from rerunning retrieval. Show original and filtered statistics with reasons for exclusions. Do not select samples to improve scores or turn unknown results into zeros.
6. **Explain rule changes.** Record versions, differences, and validation when changing questions, Gold, or scoring. Score both sides under the same rules and retain old results. Have the benchmark owner review the PR.

## Storage and Git

Keeping evidence does not require committing every record to Git.

- **Keep in Git:** reusable evaluation and analysis code, configuration, README instructions, data versions, and retrieval instructions. Small datasets, Gold, test fixtures, and result summaries may accompany the code.
- **Keep run artifacts outside Git by default:** traces, logs, raw responses, bulk results, and scripts or analysis outputs used for a single run. Store them in an ignored output directory or an archive, together with that run's commands, configuration, and execution notes.
- **Document how to retrieve evidence:** record its location or download instructions, run identifier, versions, and checksums. Add new in-repository output directories to `.gitignore` and inspect staged files before committing.

## Retrieval-only rules

- Follow the [QA pair authoring and matching rules](docs/benchmark-gold-authoring-and-matching.md). Preserve the original query and annotate File and Entity Gold separately.
- Keep the same Gold when changing the parser. Do not regenerate answers from new internal IDs or chunk boundaries. Review source or Gold changes individually and keep a change record.
- Preserve all known relevant targets from the declared annotation sources. State the scope and method, record known omissions and unresolved items, and version corrections. Recall is measured against that Gold; exhaustive whole-corpus judgment is not required.
- Preserve native result ranks in comparisons. Use the same result-count and length limits, and record the Gold, scoring rules, and scorer versions actually used.

## Maintaining these rules

Keep this file limited to requirements shared across tasks. State when a rule applies, what it requires, and how to check it. Put fields and procedures in dedicated guides, and run-specific configuration and findings in run records. Describe unimplemented features as requirements, not existing capabilities.
