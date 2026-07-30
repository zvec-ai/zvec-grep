import type { Content, ContentKind } from "../types.js";

export type RankingCandidate = {
  id: string;
  content: Content;
};

export type RankingScore = {
  id: string;
  score: number;
};

export interface RankingModel {
  readonly info: Readonly<{
    reference: string;
    provider: string;
    name: string;
    inputKinds: readonly ContentKind[];
  }>;

  rank(
    query: Content,
    candidates: readonly RankingCandidate[],
  ): Promise<RankingScore[]>;

  dispose(): Promise<void>;
}
