import { EngineError } from "../errors.js";

export const LOCAL_EMBEDDING_CONCURRENCY_ENV =
  "ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY";

const LEGACY_LLAMA_PARALLELISM_ENV = "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM";
const DEFAULT_PARALLELISM_CAP = 8;
const BYTES_PER_MIB = 1024 * 1024;
const CONTEXT_VRAM_MB = 150;
const GPU_VRAM_BUDGET_RATIO = 0.25;

export function resolveLocalEmbeddingParallelismOverride(
  options: { embeddingConcurrency?: number; legacyLlama?: boolean } = {},
): number | undefined {
  if (options.embeddingConcurrency !== undefined) {
    if (
      !Number.isSafeInteger(options.embeddingConcurrency) ||
      options.embeddingConcurrency < 1
    ) {
      throw new EngineError(
        "Embedding concurrency requires a positive integer",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.INVALID_EMBEDDING_CONCURRENCY",
        },
      );
    }
    return Math.min(DEFAULT_PARALLELISM_CAP, options.embeddingConcurrency);
  }
  let name = LOCAL_EMBEDDING_CONCURRENCY_ENV;
  let value = process.env[name]?.trim() ?? "";
  if (!value && options.legacyLlama) {
    name = LEGACY_LLAMA_PARALLELISM_ENV;
    value = process.env[name]?.trim() ?? "";
  }
  if (!value) {
    return undefined;
  }

  if (!/^0*[1-9]\d*$/.test(value)) {
    process.stderr.write(
      `zvec-grep warning: invalid ${name}="${value}", using automatic parallelism.\n`,
    );
    return undefined;
  }

  return Math.min(DEFAULT_PARALLELISM_CAP, Number(value));
}

export async function resolveLocalEmbeddingParallelism(options: {
  override?: number;
  gpu: boolean;
  getVramState?: () => Promise<{ free: number }>;
}): Promise<number> {
  if (options.override !== undefined) {
    return options.override;
  }
  if (!options.gpu || !options.getVramState) {
    return 1;
  }

  try {
    const { free } = await options.getVramState();
    if (!Number.isFinite(free) || free < 0) {
      return 2;
    }
    const freeMb = free / BYTES_PER_MIB;
    return Math.max(
      1,
      Math.min(
        DEFAULT_PARALLELISM_CAP,
        Math.floor((freeMb * GPU_VRAM_BUDGET_RATIO) / CONTEXT_VRAM_MB),
      ),
    );
  } catch {
    return 2;
  }
}
