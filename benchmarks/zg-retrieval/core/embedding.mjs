import assert from "node:assert/strict";

export const REMOTE_EMBEDDING_MODEL = "qwen/qwen3.7-text-embedding";

export function embeddingModel(configuredModel, profile = "local") {
  if (/^local\/[a-z0-9][a-z0-9.-]*$/.test(profile)) return profile;
  assert.ok(
    ["local", "remote"].includes(profile),
    `unknown embedding profile: ${profile}`,
  );
  return profile === "remote" ? REMOTE_EMBEDDING_MODEL : configuredModel;
}

/** Remote benchmark runs require an explicit destination and a credential. */
export function embeddingRuntime(model, environment = process.env) {
  if (!model.startsWith("qwen/"))
    return { remote: false, indexArguments: [], grantArguments: () => [] };

  assert.ok(
    environment.ZVEC_GREP_API_KEY?.trim(),
    "ZVEC_GREP_API_KEY is required for remote embedding",
  );
  const endpoint = environment.ZVEC_GREP_ENDPOINT?.trim();
  assert.ok(endpoint, "ZVEC_GREP_ENDPOINT is required for remote embedding");
  assert.equal(
    new URL(endpoint).protocol,
    "https:",
    "remote embedding endpoint must use HTTPS",
  );
  return {
    remote: true,
    indexArguments: ["--endpoint", endpoint, "--allow-remote"],
    grantArguments: (root) => [
      "--auth",
      "grant",
      root,
      "--capability",
      "embedding",
      "--scope",
      "workspace",
      "--embedding",
      model,
      "--endpoint",
      endpoint,
    ],
  };
}
