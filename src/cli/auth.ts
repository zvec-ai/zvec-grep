import { resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import {
  createZvecGrep,
  type CreateZvecGrepOptions,
  type EmbeddingModelInfo,
  type ZvecGrepInfoResult,
} from "../index.js";
import {
  readGlobalConfig,
  type EmbeddingRuntimeConfig,
} from "../engine/config.js";
import {
  getEmbeddingModelCatalogEntry,
  listEmbeddingModels,
  resolveEmbeddingReference,
} from "../engine/models/index.js";
import { readWorkspaceManifest } from "../engine/manifest.js";
import {
  findNearestWorkspace,
  workspaceIndexLocation,
} from "../engine/service/root.js";
import { createEmbeddingModelForIdentity } from "../engine/service/index.js";
import {
  RemoteEmbeddingAuthorizationManager,
  RemoteEmbeddingAuthorizationStore,
  createRemoteEmbeddingTarget,
  formatRemoteEmbeddingAuthorizationPrompt,
  remoteEmbeddingDisclosureData,
  withRemoteEmbeddingOperationPermit,
  type RemoteEmbeddingAuthorizationPlan,
  type RemoteEmbeddingOperationPermit,
} from "../authorization/index.js";
import type { WorkspaceIndexEmbeddingSchema } from "../engine/types.js";
import { printRemoteEmbeddingAuthorizationStatus } from "./format/status.js";
import { createServiceOptions } from "./commands.js";
import type { ParsedArgs, CliOptions } from "./types.js";

export async function runAuth(parsed: ParsedArgs): Promise<void> {
  const requestedRoot = resolve(parsed.positionals[0] ?? process.cwd());
  const root = findNearestWorkspace(requestedRoot)?.root ?? requestedRoot;
  const store = authorizationStore(parsed.options);
  if (parsed.options.authAction === "status") {
    const status = await store.status(root);
    printRemoteEmbeddingAuthorizationStatus(root, status, parsed.options);
    return;
  }
  if (parsed.options.authAction === "revoke") {
    const revoked = await store.revokeAll(root);
    console.log(
      revoked > 0
        ? `Revoked ${revoked} Remote Embedding Workspace grant(s).`
        : "No Remote Embedding Workspace grants found.",
    );
    return;
  }
  if (parsed.options.authAction !== "grant") {
    throw new Error("zg --auth requires grant, status, or revoke");
  }

  const serviceOptions = createServiceOptions(parsed.options, root);
  const service = await createZvecGrep(serviceOptions);
  try {
    const info = await service.info({ root });
    const schema = resolveAuthorizationSchema(
      configuredEmbeddingReference(
        parsed.options,
        info.workspaceIndex?.embedding,
      ),
      info.workspaceIndex?.embedding,
    );
    if (!schema) {
      throw new Error(
        "No embedding model is available. Pass --embedding <remote/model> or build an index first.",
      );
    }
    if (schema.provider === "local") {
      throw new Error("Local embedding models do not require authorization.");
    }
    const modelInfo = await embeddingModelInfo(
      schema,
      serviceOptions,
      workspaceRuntimeFromInfo(info),
    );
    const endpoint = modelInfo.endpoint;
    if (endpoint === undefined) {
      throw new Error(
        `Embedding model ${modelInfo.reference} did not provide a remote endpoint.`,
      );
    }
    const target = await createRemoteEmbeddingTarget({
      roots: info.workspaceIndex?.rootPaths.map(
        (item) => item.absolutePath,
      ) ?? [root],
      provider: modelInfo.provider,
      model: modelInfo.name,
      endpoint,
    });
    const manager = new RemoteEmbeddingAuthorizationManager(store);
    const plan: RemoteEmbeddingAuthorizationPlan = {
      operation: "query_and_index",
      target,
      disclosure: { queryText: true, workspaceContent: "full" },
      reason: "index_create",
      grantPath: store.grantPath(target),
    };
    await manager.grant(plan, "workspace");
    printRemoteEmbeddingAuthorizationStatus(
      root,
      await store.status(root),
      parsed.options,
    );
  } finally {
    await service.close();
  }
}

export function authorizationStore(
  options: CliOptions,
): RemoteEmbeddingAuthorizationStore {
  return new RemoteEmbeddingAuthorizationStore({
    signingKeyPath: createServiceOptions(options, undefined)
      .authorizationSigningKeyPath,
  });
}

export async function authorizeCliPlan(
  plan: RemoteEmbeddingAuthorizationPlan,
  options: CliOptions,
  alternative?: "local_search",
): Promise<{
  authorization?: RemoteEmbeddingOperationPermit;
  alternative?: "local_search";
}> {
  const manager = new RemoteEmbeddingAuthorizationManager(
    authorizationStore(options),
  );
  const existing = await manager.existingWorkspacePermit(plan);
  if (existing) return { authorization: existing };
  if (options.allowRemote) {
    return {
      authorization: await manager.grant(plan, "once"),
    };
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(
      [
        "Remote Embedding authorization is required.",
        "Re-run with --allow-remote, or grant Workspace authorization:",
        "  zg --auth grant --capability embedding --scope workspace",
      ].join("\n"),
    );
  }

  console.error(
    formatRemoteEmbeddingAuthorizationPrompt({
      workspaceRoots: plan.target.workspaceRoots,
      provider: plan.target.provider,
      model: plan.target.model,
      endpoint: plan.target.endpoint,
      data: remoteEmbeddingDisclosureData(plan.disclosure),
    }),
  );
  console.error("");
  console.error("1. Allow once");
  console.error("2. Allow for this workspace");
  if (alternative === "local_search") console.error("3. Use FTS only");
  const cancelChoice = alternative ? 4 : 3;
  console.error(`${cancelChoice}. Cancel`);
  const readline = createInterface({
    input: process.stdin,
    output: process.stderr,
  });
  try {
    const answer = (
      await readline.question(`Choose [1-${cancelChoice}]: `)
    ).trim();
    if (answer === "1") {
      return { authorization: await manager.grant(plan, "once") };
    }
    if (answer === "2") {
      return { authorization: await manager.grant(plan, "workspace") };
    }
    if (answer === "3" && alternative) {
      return { alternative };
    }
    throw new Error(
      "Remote Embedding authorization was declined. No remote data was sent.",
    );
  } finally {
    readline.close();
  }
}

export function resolveAuthorizationSchema(
  reference: string | undefined,
  existing: WorkspaceIndexEmbeddingSchema | null | undefined,
): Pick<WorkspaceIndexEmbeddingSchema, "provider" | "model"> | undefined {
  if (!reference) return existing ?? undefined;
  const separator = reference.indexOf("/");
  if (separator <= 0 || separator === reference.length - 1) {
    throw new Error(`Invalid embedding reference: ${reference}`);
  }
  const provider = reference.slice(0, separator);
  const model = reference.slice(separator + 1);
  return {
    provider,
    model,
  };
}

export async function embeddingModelInfo(
  schema: Pick<WorkspaceIndexEmbeddingSchema, "provider" | "model">,
  options: CreateZvecGrepOptions,
  workspaceRuntime: EmbeddingRuntimeConfig = {},
): Promise<EmbeddingModelInfo> {
  const model = createEmbeddingModelForIdentity(
    { provider: schema.provider, name: schema.model },
    options,
    workspaceRuntime,
  );
  try {
    return model.info;
  } finally {
    await model.dispose();
  }
}

export function assertEmbeddingModelCompatible(
  existing: WorkspaceIndexEmbeddingSchema | null | undefined,
  requested:
    Pick<WorkspaceIndexEmbeddingSchema, "provider" | "model"> | undefined,
  rebuild: boolean,
): void {
  if (!existing || !requested || rebuild) return;
  if (
    existing.provider === requested.provider &&
    existing.model === requested.model
  ) {
    return;
  }
  throw new Error(
    [
      "Embedding model does not match the existing index.",
      `Existing model: ${existing.provider}/${existing.model}`,
      `Requested model: ${requested.provider}/${requested.model}`,
      "Re-run with --rebuild to change the embedding model.",
    ].join("\n"),
  );
}

export function configuredEmbeddingReference(
  options: CliOptions,
  existing?: WorkspaceIndexEmbeddingSchema | null,
): string | undefined {
  return resolveEmbeddingReference({
    explicit: options.embedding,
    existing: existing ? `${existing.provider}/${existing.model}` : undefined,
    globalDefault: readGlobalConfig().defaults?.embedding,
  });
}

export function requireEmbeddingModelCatalogEntry(reference: string) {
  const entry = getEmbeddingModelCatalogEntry(reference);
  if (entry) {
    return entry;
  }
  throw new Error(
    [
      `Unsupported embedding model: ${reference}`,
      "Run `zg --help models` to list supported models.",
    ].join("\n"),
  );
}

export function unsupportedRemoteEmbeddingProvider(
  reference: string,
  message = "Unsupported remote embedding provider",
): Error {
  const providers = [
    ...new Set(
      listEmbeddingModels()
        .map((model) => model.provider)
        .filter((provider) => provider !== "local"),
    ),
  ];
  return new Error(
    [
      `${message}: ${reference}`,
      "Supported remote embedding providers:",
      ...providers.map((provider) => `  ${provider}`),
    ].join("\n"),
  );
}

export function workspaceRuntimeFromInfo(
  info: ZvecGrepInfoResult,
): EmbeddingRuntimeConfig {
  const workspaceIndex = info.workspaceIndex;
  if (!workspaceIndex) return {};
  const location = workspaceIndexLocation(info.root);
  return readWorkspaceManifest(location.home)?.embeddingRuntime ?? {};
}

export function assertRequestedEndpointCompatible(
  info: ZvecGrepInfoResult,
  workspaceRuntime: EmbeddingRuntimeConfig,
  requestedEndpoint: string | undefined,
  rebuild: boolean,
): void {
  if (
    rebuild ||
    !info.workspaceIndex?.embedding ||
    requestedEndpoint === undefined ||
    workspaceRuntime.endpoint === requestedEndpoint
  ) {
    return;
  }
  throw new Error(
    "The requested embedding endpoint differs from the workspace snapshot. Run zg --index --endpoint <url> --rebuild first.",
  );
}

export { withRemoteEmbeddingOperationPermit };
export type {
  RemoteEmbeddingAuthorizationPlan,
  RemoteEmbeddingOperationPermit,
};
