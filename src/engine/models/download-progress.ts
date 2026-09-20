import type { EmbeddingModelProgress } from "./embeddings.js";

export type ModelArtifactDownloadProgress = {
  artifact: string;
  downloadedBytes: number;
};

export type ModelDownloadProgressReporter = {
  start(): void;
  setDownloadPlan(artifacts: readonly { path: string; size: number }[]): void;
  report(progress: ModelArtifactDownloadProgress): void;
  warning(message: string): boolean;
  finish(): void;
};

export function createModelDownloadProgressReporter(
  model: string,
  onProgress?: (progress: EmbeddingModelProgress) => void,
): ModelDownloadProgressReporter {
  const artifacts = new Map<
    string,
    { downloadedBytes: number; totalBytes: number }
  >();

  const reportDownload = (): void => {
    const values = [...artifacts.values()];
    const downloadedBytes = values.reduce(
      (sum, artifact) => sum + artifact.downloadedBytes,
      0,
    );
    const totalBytes = values.reduce(
      (sum, artifact) => sum + artifact.totalBytes,
      0,
    );
    onProgress?.({
      stage: "downloading",
      model,
      downloadedBytes,
      totalBytes,
    });
  };

  return {
    start() {
      onProgress?.({ stage: "preparing", model });
    },
    setDownloadPlan(plannedArtifacts) {
      artifacts.clear();
      for (const artifact of plannedArtifacts) {
        artifacts.set(artifact.path, {
          downloadedBytes: 0,
          totalBytes: artifact.size,
        });
      }
    },
    report(progress) {
      const artifact = artifacts.get(progress.artifact);
      if (!artifact) {
        return;
      }
      artifact.downloadedBytes = progress.downloadedBytes;
      reportDownload();
    },
    warning(message) {
      if (!onProgress) {
        return false;
      }
      onProgress({ stage: "warning", model, message });
      return true;
    },
    finish() {
      onProgress?.({ stage: "ready", model });
    },
  };
}
