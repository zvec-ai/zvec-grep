# BrowseComp-Plus paired report

- Run: `20260826-095022`
- Suite: `study`
- Model: `gpt-5.6-sol`
- Reasoning: `high`
- Completed cases: 100 / 100
- Completed trials: 300 / 300 (3 per case)

## Primary results

Every completed Baseline and Treatment trial is included in the averages. Changes are calculated as Treatment relative to Baseline.

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Accuracy (%) | 98.67 | 99.00 | 0.33 pp | - |
| Average input tokens | 1,675,379.50 | 1,046,115.41 | -629,264.09 | −37.56% |
| Average cached input tokens | 1,551,610.03 | 949,456.21 | -602,153.81 | −38.81% |
| Average output tokens | 6,769.61 | 4,478.43 | -2,291.18 | −33.85% |
| Average reasoning output tokens | 3,646.84 | 2,388.62 | -1,258.23 | −34.50% |
| Average total tokens | 1,682,149.11 | 1,050,593.84 | -631,555.27 | −37.54% |
| Average tool calls | 25.42 | 14.36 | -11.06 | −43.52% |
| Average command calls | 25.42 | 11.06 | -14.36 | −56.48% |
| Average zvec-grep calls | 0.00 | 3.30 | 3.30 | N/A |
| Average Agent time (seconds) | 259.39 | 159.31 | -100.08 | −38.58% |
| Average document ID mentions | 3,975.13 | 734.73 | -3,240.40 | −81.52% |

## Quality outcomes

| Outcome | Trials |
| --- | ---: |
| Both correct | 294 |
| Baseline only correct | 2 |
| Treatment only correct | 3 |
| Neither correct | 1 |


## Both-correct analysis

Resource use is most directly comparable when both conditions answer correctly; otherwise, it may reflect an unsuccessful trajectory—for example, premature stopping or prolonged, unfocused searching when the model cannot resolve the task—rather than retrieval efficiency. This secondary view therefore compares resource and retrieval behavior on the 294 paired trials where both Baseline and Treatment answered correctly. The primary results above still include all completed trials.

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Average input tokens | 1,594,196.95 | 964,559.06 | -629,637.89 | −39.50% |
| Average cached input tokens | 1,473,803.32 | 871,141.88 | -602,661.44 | −40.89% |
| Average output tokens | 6,351.69 | 4,292.42 | -2,059.27 | −32.42% |
| Average reasoning output tokens | 3,309.61 | 2,287.82 | -1,021.79 | −30.87% |
| Average total tokens | 1,600,548.65 | 968,851.49 | -631,697.16 | −39.47% |
| Average tool calls | 25.04 | 13.86 | -11.18 | −44.64% |
| Average command calls | 25.04 | 10.63 | -14.41 | −57.54% |
| Average zvec-grep calls | 0.00 | 3.23 | 3.23 | N/A |
| Average Agent time (seconds) | 243.99 | 151.55 | -92.44 | −37.89% |
| Average document ID mentions | 3,985.45 | 687.30 | -3,298.15 | −82.75% |
| Evidence recall (%) | 92.72 | 94.63 | 1.91 pp | - |
| Evidence hit rate (%) | 100.00 | 100.00 | 0.00 pp | - |
| Gold recall (%) | 96.07 | 97.31 | 1.25 pp | - |
| Gold hit rate (%) | 99.66 | 100.00 | 0.34 pp | - |
| Average tool-interaction batches | 19.58 | 12.81 | -6.77 | −34.59% |
| Average batch to first evidence | 4.07 | 1.90 | -2.17 | −53.38% |
| Average batch to first gold | 7.83 | 3.35 | -4.48 | −57.23% |


## Case-level distribution

Each case is compared using the mean of its trials. Negative median change means lower use with zvec-grep.

| Metric | Improved | Tied | Regressed | Median case change (%) |
| --- | ---: | ---: | ---: | ---: |
| Input tokens | 67 | 0 | 33 | -25.76 |
| Tool calls | 84 | 2 | 14 | -30.00 |
| Agent time | 76 | 0 | 24 | -20.71 |

## Retrieval diagnostics

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Evidence recall (%) | 92.49 | 94.54 | 2.05 pp | - |
| Evidence hit rate (%) | 100.00 | 100.00 | 0.00 pp | - |
| Gold recall (%) | 95.92 | 97.37 | 1.44 pp | - |
| Gold hit rate (%) | 99.67 | 100.00 | 0.33 pp | - |
| Average tool-interaction batches | 19.89 | 13.17 | -6.72 | −33.80% |
| Average batch to first evidence | 4.13 | 1.89 | -2.24 | −54.24% |
| Average batch to first gold | 7.88 | 3.47 | -4.40 | −55.90% |

## Tool behavior

| Metric | Value |
| --- | ---: |
| Treatment trials using zvec-grep | 300 / 300 (100.00%) |
| Treatment zvec-grep calls | 989 |
| Successful zvec-grep calls | 988 |
| Failed zvec-grep calls | 1 |
| Successful calls with empty output | 0 |
| Baseline direct `zg` commands | 0 |
| Baseline zvec-grep MCP calls | 0 |


## zvec-grep index preparation

Index preparation is measured separately and excluded from Agent execution metrics.

| Metric | Value |
| --- | ---: |
| Build time (seconds) | 9721.01 |
| Index size (bytes) | 7,897,230,894 |
| Index statistics | `{"corpus_files": 100195, "entities": 213103, "indexed_files": 100195, "truncated_fragments": 0}` |

## Runtime preparation

| Phase | Wall seconds |
| --- | ---: |
| End-to-end preparation | 22.92 |
| Server startup | 0.87 |
| Profile preparation | 0.29 |
| `zg --install` | 0.22 |
| Runtime verification and index warmup | 21.57 |

## Environment

| Setting | Value |
| --- | --- |
| Operating system | Ubuntu 24.04.4 LTS |
| Kernel / platform | Linux-6.8.0-136-generic-x86_64-with-glibc2.39 |
| Architecture | x86_64 |
| CPU | Intel(R) Xeon(R) 6982P-C |
| CPU count | 16 available / 16 logical |
| Python | 3.12.3 |
| Codex | codex-cli 0.147.0 |
| Codex executable | /root/.codex/packages/standalone/releases/0.147.0-x86_64-unknown-linux-musl/bin/codex |
| Codex sandbox | workspace-write |
| Web search | disabled |
| History persistence | none |
| Query dataset | Tevatron/browsecomp-plus@144cff8e35b5eaef7e526346aa60774a9deb941f |
| Query split | test |
| Corpus | Tevatron/browsecomp-plus-corpus@b27b02bc3e45511b8b82a13e6f90ce761df726f6 |
| Corpus split | train |
| zvec-grep | 0.2.0 |
| Embedding | qwen/qwen3.7-text-embedding |
| FTS tokenizer | jieba |
| Index embedding concurrency | 60 |
| Configured embedding device | auto |
| Maximum indexed file size | 64M |
| MCP transport | http |
| MCP tool timeout | 600 seconds |
| zvec-grep server | http://127.0.0.1:7999/mcp |

## Evaluator

- Model: `gpt-5.6-sol`
- Reasoning: `high`
- Evaluator usage and time are excluded from Agent execution metrics.


| Workload | Calls | Input tokens | Cached input tokens | Output tokens | Reasoning output tokens | Wall seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Answer judgements | 600 | 6532938 | 5015808 | 29159 | 0 | 3184.38 |
| zvec-grep usage audits | 0 | 0 | 0 | 0 | 0 | 0.00 |
| Total | 600 | 6532938 | 5015808 | 29159 | 0 | 3184.38 |
