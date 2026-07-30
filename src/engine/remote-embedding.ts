import { EngineError } from "./errors/index.js";

const DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";
const DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding";

export function resolveRemoteEmbeddingEndpoint(
  reference: string,
  endpoint?: string,
): string {
  if (endpoint !== undefined) {
    return endpoint.trim();
  }
  if (
    reference === "qwen/text-embedding-v4" ||
    reference === "qwen/qwen3.7-text-embedding"
  ) {
    return DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT;
  }
  if (reference === "qwen/qwen3-vl-embedding") {
    return DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT;
  }
  throw new EngineError("Remote embedding endpoint is not implemented", {
    code: "ZVEC_GREP.ENGINE.MODELS.REMOTE_ENDPOINT_NOT_IMPLEMENTED",
    context: `reference=${reference}`,
  });
}
