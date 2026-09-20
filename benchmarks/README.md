<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# Benchmarks

Each evaluation measures how zvec-grep affects agent answer quality and
retrieval efficiency across different workloads. Every benchmark keeps its
inputs and dependencies pinned, provides a local runner, separates generated
artifacts from source, and documents its protocol in a dedicated README.

See benchmark-specific README for setup and execution instructions.

## Benchmark suites

| Benchmark | Description | Agent | Study scope |
| --- | --- | --- | --- |
| [BrowseComp-Plus](browse-comp-plus/README.md) | Evaluates multi-document evidence retrieval and answer accuracy over a large, fixed corpus | Codex | 100 cases |
| [SWE-QA-Bench](swe-qa-bench/README.md) | Evaluates repository-level, cross-file, and multi-hop software-engineering question answering | Claude Code · Claude Opus 5 (high) | 20 tasks |

## Evaluation protocol

All benchmarks use controlled, paired A/B evaluations. For each case, the
baseline and treatment profiles keep the task inputs, agent, model, environment,
and limits fixed.

- **Baseline:** the agent uses its standard tools and instructions.
- **Treatment (zvec-grep):** the same agent additionally receives a prepared
  index, zvec-grep tools, and standard usage guidance.

The **only intended difference** between paired runs is access to zvec-grep.
To keep the comparison focused on agent behavior, index preparation is measured
and reported separately.

## Evaluation metrics

Where applicable, benchmarks measure:

| Metric | What it measures | Better |
| --- | --- | --- |
| Answer quality | Task-specific judge score or accuracy | Higher |
| Input tokens | Model input consumed during agent execution | Lower |
| Tool calls | Recorded tool invocations during agent execution | Lower |
| Agent wall time | Agent execution time (excluding separately reported zvec-grep index preparation time) | Lower |

Additional metrics may be reported when relevant. Completion status and raw
trajectories may also be retained for auditing and diagnosis.

## Evaluation considerations

Each benchmark's README provides its complete reproduction instructions. While
building and running these evaluations, we encountered several easy-to-miss
pitfalls; we share the most important ones below:

- **Keep comparisons like for like.** To isolate the effect of zvec-grep,
  Baseline and Treatment should use the same model and version, reasoning
  settings, agent framework, base prompt, shared tools, task set, environment,
  and limits. The only Treatment-specific additions should be a prepared index,
  zvec-grep tools, and standard usage instructions.

  For example, a stronger model under Baseline may outperform a weaker model
  with zvec-grep because its reasoning ability and prior knowledge can
  compensate for less effective retrieval. Conversely, pairing zvec-grep with
  the stronger model gives Treatment an inherent model advantage over a weaker
  Baseline. Neither comparison isolates the effect of zvec-grep.
- **Preserve general-purpose behavior.** Use general-purpose prompts and tool
  instructions that apply across tasks, rather than wording tailored to a
  particular benchmark, dataset, or expected answer. Guidance optimized for
  one scenario may not generalize to another.

  For example, instructing the agent to always use zvec-grep instead of grep on
  tasks well suited to semantic retrieval can overstate its apparent benefit,
  while the same rule may hurt tasks where an exact identifier or known keyword
  is better handled by native grep. Rules that force or forbid either tool bias
  the evaluation toward particular scenarios. Give the agent access to the
  available tools and let it decide how and when to use them.
- **Account for stochasticity.** Even with identical configurations, model
  behavior is inherently stochastic: at the same decision point, an agent may
  choose a different tool, query, or next action, and those choices can compound
  into a different trajectory and outcome. This variation affects answer
  quality, token use, tool calls, and time.

  We observed that the impact was not uniform across metrics. Answer quality
  tended to remain at a similar level across runs, while token use and tool
  calls often varied more as trajectories diverged. One possible explanation is
  that different exploration paths can add or remove entire sequences of search
  and inspection, substantially changing resource use, while model capability
  helps the agent judge whether it has enough evidence and complete the task
  despite taking a different path. Wall time was also affected by model-provider
  latency and other external conditions, making it harder to control.

  To reduce uncertainty, run the same number of independent trials for Baseline
  and Treatment. With more trials, unusually favorable or unfavorable runs have
  less influence, producing a more precise estimate of the mean.
- **Validate tool access.** A successful installation does not guarantee that
  zvec-grep is visible or callable in the agent's environment. Before
  evaluation, use a separate smoke test to verify that an actual call returns
  valid results instead of failing immediately. In our experience, when an
  agent's first call to an external tool fails, it often stops trying that tool
  and falls back to other methods for the rest of the run. A setup or invocation
  error can therefore affect the entire trajectory. During evaluation, however,
  whether and when to use zvec-grep should remain the agent's decision; choosing
  not to use it is not itself a setup failure.
- **Prevent evaluation leakage.** Keep reference answers, previous outputs,
  reports, and other evaluation artifacts outside the agent-visible workspace.
  For example, if a reference answer or earlier report is left in the workspace
  or indexed corpus, the agent may retrieve it directly and appear to solve the
  task. Likewise, a judge that sees profile labels or tool traces may score
  according to expectations rather than the answer itself. Keep the judge blind
  to whether an answer came from Baseline or Treatment and which tools were
  used, and apply the same rubric to both.

  Leakage can also predate the evaluation: widely used benchmark items may have
  appeared in model training data, with the relevant knowledge absorbed into
  the model's parameters. In such cases, performance may partly reflect prior
  exposure, leaving less room for retrieval to provide an incremental benefit.
  Where possible, prefer fresh or held-out tasks and disclose the risk of
  benchmark contamination.
