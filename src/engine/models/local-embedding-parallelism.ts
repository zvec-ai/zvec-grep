import { EngineError } from "../errors.js";

export const INDEX_EMBEDDING_CONCURRENCY_ENV =
  "ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY";

const LEGACY_LLAMA_PARALLELISM_ENV = "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM";
const DEFAULT_PARALLELISM_CAP = 8;
const BYTES_PER_MIB = 1024 * 1024;
const CONTEXT_VRAM_MB = 150;
const GPU_VRAM_BUDGET_RATIO = 0.25;

/** Validate an explicit backend limit without consulting the environment. */
export function normalizeLocalEmbeddingConcurrency(
  value?: number,
): number | undefined {
  const concurrency = validateEmbeddingConcurrency(value);
  return concurrency === undefined
    ? undefined
    : Math.min(DEFAULT_PARALLELISM_CAP, concurrency);
}

/** Resolve index-stage configuration; individual backends apply their own cap. */
export function resolveIndexEmbeddingConcurrencyOverride(
  options: { embeddingConcurrency?: number; legacyLlama?: boolean } = {},
): number | undefined {
  if (options.embeddingConcurrency !== undefined) {
    return validateEmbeddingConcurrency(options.embeddingConcurrency);
  }
  let name = INDEX_EMBEDDING_CONCURRENCY_ENV;
  let value = process.env[name]?.trim() ?? "";
  if (!value && options.legacyLlama) {
    name = LEGACY_LLAMA_PARALLELISM_ENV;
    value = process.env[name]?.trim() ?? "";
  }
  if (!value) {
    return undefined;
  }

  const concurrency = Number(value);
  if (!/^0*[1-9]\d*$/.test(value) || !Number.isSafeInteger(concurrency)) {
    process.stderr.write(
      `zvec-grep warning: invalid ${name}="${value}", using automatic parallelism.\n`,
    );
    return undefined;
  }

  return concurrency;
}

function validateEmbeddingConcurrency(value?: number): number | undefined {
  if (value !== undefined && (!Number.isSafeInteger(value) || value < 1)) {
    throw new EngineError("Embedding concurrency requires a positive integer", {
      code: "ZVEC_GREP.ENGINE.MODELS.INVALID_EMBEDDING_CONCURRENCY",
    });
  }
  return value;
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
