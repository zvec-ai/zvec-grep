export const PILOT_PROTOCOL = "retrieval-pilots-mcp-full-v1";
export const PILOT_PREVIEW = "full";
export const PILOT_QA_SCORING = Object.freeze({
  version: 2,
  entity_match: "contains",
  range: "source_bytes",
  evidence_sets: "any_complete_set",
});
export const PILOT_FILE_RECALL_METRICS = [1, 5, 10].map(
  (k) => `file_recall_at_${k}`,
);

// Suite metadata lives here; original queries, gold labels, and source hashes
// remain in the corresponding data/*.json lock files.
// targetKind selects document qrels or unique source-file paths; a null
// breakdownBy omits the grouped score table.
export const PILOT_SUITES = {
  beir: {
    suite: "beir20",
    goldVersion: 2,
    lockFile: "beir20.json",
    taskCount: 20,
    model: "local/potion-multilingual-128m",
    targetKind: "qrels",
    breakdownBy: "dataset",
    indexTimeoutMinutes: 180,
    report: {
      title: "BEIR / four datasets (test)",
      overview: "BEIR / four datasets",
      note: [
        "BEIR uses original test queries and qrels with each dataset's complete corpus.",
        "Each document is one Markdown file. ArguAna excludes the query's own",
        "corpus document before indexing. Original graded qrels are retained;",
        "current file metrics treat every positive grade as relevant.",
      ].join(" "),
    },
  },
  duretrieval: {
    suite: "duretrieval10",
    goldVersion: 1,
    lockFile: "duretrieval10.json",
    taskCount: 10,
    model: "local/potion-multilingual-128m",
    targetKind: "qrels",
    breakdownBy: null,
    indexTimeoutMinutes: 300,
    report: {
      title: "DuRetrieval (C-MTEB dev)",
      overview: "DuRetrieval / Chinese web search",
      note: [
        "DuRetrieval uses ten unchanged Chinese dev queries and qrels against",
        "the complete pinned C-MTEB corpus subset, one passage per Markdown file.",
        "These pilot scores are not official full-corpus DuReader scores.",
      ].join(" "),
    },
  },
  quarry: {
    suite: "quarry20",
    goldVersion: 1,
    lockFile: "quarry20.json",
    taskCount: 20,
    model: "local/potion-code-16m-v2",
    targetKind: "positive_units",
    breakdownBy: "language",
    indexTimeoutMinutes: 40,
    fileExtensions: {
      Go: "go",
      Python: "py",
      Rust: "rs",
      JavaScript: "js",
      TypeScript: "ts",
      Java: "java",
      "C#": "cs",
      C: "c",
    },
    report: {
      title: "Quarry / eight languages (preimage)",
      overview: "Quarry / eight languages",
      note: [
        "Quarry's function-level positives are projected to unique files.",
        "These are pilot file metrics, not the official Quarry function recall.",
      ].join(" "),
    },
  },
};

export const PILOT_NAMES = Object.keys(PILOT_SUITES);
// Reports carry the pinned suite ID, while CLI arguments and CI jobs use names.
export const PILOT_SUITES_BY_ID = Object.fromEntries(
  Object.values(PILOT_SUITES).map((config) => [config.suite, config]),
);
