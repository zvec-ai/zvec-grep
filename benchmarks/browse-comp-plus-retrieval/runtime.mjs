import { run } from "../zg-retrieval/core/io.mjs";

export function candidateCommand(candidate, args) {
  return { command: candidate.cli, args };
}

export function runCandidate(candidate, args, options) {
  return run(candidate.cli, args, options);
}
