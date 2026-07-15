import type { Content, ContentKind } from "../types.js";
import type { ModelRef } from "./types.js";

export type RankingCandidate = {
  id: string;
  content: Content;
};

export type RankingScore = {
  id: string;
  score: number;
};

export interface RankingModel {
  readonly ref: ModelRef;
  readonly supportedContentKinds: readonly ContentKind[];

  rank(
    query: Content,
    candidates: readonly RankingCandidate[],
  ): Promise<RankingScore[]>;
}
