import { resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import {
  createZvecGrep,
  type CreateZvecGrepOptions,
  type FileScanDiagnostics,
  type IndexProgress,
  type RootPath,
  type ZvecGrepContextOptions,
  type ZvecGrepContextRoute,
  type ZvecGrep,
  type ZvecGrepInfoResult,
} from "../index.js";
import { globalConfigPath, updateGlobalConfig } from "../engine/config.js";
import { listEmbeddingModels } from "../engine/models/index.js";
import { DaemonClient } from "../client/daemon-client.js";
import {
  resolveDirectSearchPolicy,
  resolveServerSearchPolicy,
} from "../client/search-policy.js";
import {
  resolveClientMode,
  resolveServerUrl,
  routeByMode,
} from "../client/mode-router.js";
import { serverStatus } from "../daemon/server-controller.js";
import { findNearestWorkspace } from "../engine/service/root.js";
import type { ParsedArgs, CliOptions } from "./types.js";
import {
  contextWarningLines,
  printCliContextResult,
} from "./format/context.js";
import {
  printExploreResult,
  printNeighborhoodResult,
} from "./format/explore.js";
import { printDebug } from "./format/debug.js";
import { createIndexProgressReporter } from "./format/progress.js";
import {
  printWorkspaceInfo,
  printIndexPathFilterTip,
  printIndexResult,
  printIndexScanDiagnostics,
  printNoIndexableFilesTip,
  printServerIndexInfo,
} from "./format/status.js";
import {
  planRemoteIndexAuthorization,
  planRemoteSearchAuthorization,
} from "../authorization/index.js";
import {
  indexCompletionFromStatus,
  indexStatusNeedsRefresh,
} from "../engine/index-status.js";
import type { NormalizedSearchInput } from "../mcp/input-normalization.js";
import { indexProgressFromMessage } from "../index-progress.js";
import { normalizeManagedRgInput } from "./managed-rg.js";
import {
  INCOMPATIBLE_SERVER_SEARCH_MESSAGE,
  parseServerSearchResponse,
} from "./server-search.js";
import { runInstall, runUninstall } from "./install.js";
import {
  runAuth,
  authorizationStore,
  authorizeCliPlan,
  resolveAuthorizationSchema,
  embeddingModelInfo,
  configuredEmbeddingReference,
  workspaceRuntimeFromInfo,
  assertEmbeddingModelCompatible,
  assertRequestedEndpointCompatible,
  requireEmbeddingModelCatalogEntry,
  unsupportedRemoteEmbeddingProvider,
  withRemoteEmbeddingOperationPermit,
} from "./auth.js";

export async function runParsedCommand(parsed: ParsedArgs): Promise<void> {
  switch (parsed.command) {
    case "query":
      await runQuery(parsed);
      return;
    case "explore":
      await runExplore(parsed);
      return;
    case "callers":
    case "callees":
    case "impact":
      await runGraphNeighborhood(parsed);
      return;
    case "index":
      await runIndex(parsed);
      return;
    case "status":
      await runStatus(parsed);
      return;
    case "install":
      await runInstall(parsed);
      return;
    case "uninstall":
      await runUninstall(parsed);
      return;
    case "config":
      await runConfig(parsed);
      return;
    case "auth":
      await runAuth(parsed);
      return;
    case "server":
      await runServer(parsed);
      return;
    case "help":
    case "version":
      throw new Error(`${parsed.command} must be handled before dispatch`);
  }
}

async function runConfig(parsed: ParsedArgs): Promise<void> {
  if (parsed.positionals.length !== 1) {
    throw new Error(
      `zg config ${parsed.options.configAction === "provider-set" ? "provider" : "model"} set requires exactly one reference`,
    );
  }
  const reference = parsed.positionals[0]!;
  if (parsed.options.configAction === "provider-set") {
    if (!/^[a-z][a-z0-9_-]*$/.test(reference)) {
      throw unsupportedRemoteEmbeddingProvider(
        reference,
        "Invalid embedding provider",
      );
    }
    if (
      reference === "local" ||
      !listEmbeddingModels().some((entry) => entry.provider === reference)
    ) {
      throw unsupportedRemoteEmbeddingProvider(reference);
    }
    if (parsed.options.apiKey === undefined) {
      throw new Error("zg config provider set requires --api-key");
    }
    updateGlobalConfig({
      providers: {
        [reference]: {
          apiKey: parsed.options.apiKey,
        },
      },
    });
    console.log(`Provider config: ${reference}`);
    console.log(`Global config: ${globalConfigPath()}`);
    return;
  }

  if (parsed.options.configAction !== "model-set") {
    throw new Error("zg config requires provider set or model set");
  }
  const catalogEntry = requireEmbeddingModelCatalogEntry(reference);
  if (
    parsed.options.endpoint === undefined &&
    parsed.options.device === undefined &&
    !parsed.options.defaultModel
  ) {
    throw new Error(
      "zg config model set requires --endpoint, --device, or --default",
    );
  }
  if (
    catalogEntry.provider === "local" &&
    parsed.options.endpoint !== undefined
  ) {
    throw new Error("--endpoint is only supported for remote embedding models");
  }
  if (
    catalogEntry.provider !== "local" &&
    parsed.options.device !== undefined
  ) {
    throw new Error("--device is only supported for local embedding models");
  }
  if (parsed.options.endpoint !== undefined) {
    assertHttpEndpoint(parsed.options.endpoint);
  }
  updateGlobalConfig({
    ...(parsed.options.endpoint !== undefined ||
    parsed.options.device !== undefined
      ? {
          models: {
            [reference]: {
              ...(parsed.options.endpoint !== undefined
                ? { endpoint: parsed.options.endpoint }
                : {}),
              ...(parsed.options.device !== undefined
                ? { device: parsed.options.device }
                : {}),
            },
          },
        }
      : {}),
    ...(parsed.options.defaultModel
      ? { defaults: { embedding: reference } }
      : {}),
  });
  console.log(`Model config: ${reference}`);
  console.log(`Global config: ${globalConfigPath()}`);
}

function assertHttpEndpoint(endpoint: string): void {
  let parsed: URL;
  try {
    parsed = new URL(endpoint);
  } catch {
    throw new Error("--endpoint must be a valid HTTP(S) URL");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("--endpoint must be a valid HTTP(S) URL");
  }
}

async function runIndex(parsed: ParsedArgs): Promise<void> {
  const explicitRoot = parsed.positionals.length > 0;
  const root = resolveIndexRoot(parsed.positionals[0]);
  const rootPath = indexRootPath(root, parsed.options);
  if (parsed.positionals.length > 1) {
    throw new Error("zg index accepts at most one root path");
  }

  if (parsed.options.drop) {
    await runDropIndex(parsed, rootPath.absolutePath);
    return;
  }
  if (parsed.options.embedding) {
    requireEmbeddingModelCatalogEntry(parsed.options.embedding);
  }

  const mode = resolveClientMode(parsed.options.mode);
  await routeByMode({
    mode,
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const client = daemonClient(parsed.options);
      const progress = createIndexProgressReporter({
        color: parsed.options.color,
      });
      let result: Record<string, unknown>;
      try {
        result = await client.callTool(
          "zvec_grep_index",
          {
            root: rootPath.absolutePath,
            embedding: parsed.options.embedding,
            apiKey: parsed.options.apiKey,
            endpoint: parsed.options.endpoint,
            device: parsed.options.device,
            rebuild: parsed.options.rebuild,
            resetPaths: parsed.options.resetPaths,
            globs: parsed.options.globs,
            insensitiveGlobs: parsed.options.insensitiveGlobs,
            fileTypes: parsed.options.fileTypes,
            excludedFileTypes: parsed.options.excludedFileTypes,
            hidden: parsed.options.hidden,
            noIgnore: parsed.options.noIgnore,
            ignoreFiles: parsed.options.ignoreFiles,
            maxDepth: parsed.options.maxDepth,
            maxFileSizeBytes: parsed.options.maxFileSizeBytes,
            follow: parsed.options.follow,
            embeddingConcurrency: parsed.options.embeddingConcurrency,
            debug: parsed.options.debug,
            wait: true,
          },
          {
            onProgress: (event) => {
              const update = indexProgressFromMessage(event.message);
              if (update) {
                progress.report(update.progress);
              }
            },
            embeddingEnvironment: process.env.ZVEC_GREP_EMBEDDING?.trim(),
          },
        );
      } finally {
        progress.finish();
      }
      console.log(`Workspace index: ${String(result.state ?? "submitted")}`);
      console.log(`Root: ${String(result.root ?? rootPath.absolutePath)}`);
      console.log(`Job: ${String(result.job_id ?? "unknown")}`);
      if (parsed.options.debug) {
        printIndexScanDiagnostics(
          result.scan_diagnostics as FileScanDiagnostics | undefined,
        );
      }
      if (result.state === "failed") {
        throw new Error(serverIndexFailureMessage(result));
      }
      if (result.state === "succeeded") {
        const status = (await client
          .callTool("zvec_grep_index_status", {
            root: rootPath.absolutePath,
          })
          .catch(() => undefined)) as
          Parameters<typeof printServerIndexInfo>[0] | undefined;
        if (status?.persistent.files?.scanned === 0) {
          printNoIndexableFilesTip(parsed.options);
        }
      }
    },
    direct: () => runDirectIndex(parsed, rootPath, explicitRoot),
  });
}

function serverIndexFailureMessage(result: Record<string, unknown>): string {
  const error = result.error;
  if (error && typeof error === "object") {
    const code = (error as Record<string, unknown>).code;
    const message = (error as Record<string, unknown>).message;
    if (typeof message === "string" && message.length > 0) {
      if (
        typeof code !== "string" ||
        code.length === 0 ||
        message.includes(`[${code}]`)
      ) {
        return message;
      }
      return `[${code}] ${message}`;
    }
    if (typeof code === "string" && code.length > 0) {
      return `[${code}] Index job failed.`;
    }
  }
  return `Index job ${String(result.job_id ?? "unknown")} failed. Run zg status --mode server for details.`;
}

async function runDirectIndex(
  parsed: ParsedArgs,
  rootPath: RootPath,
  explicitRoot: boolean,
): Promise<void> {
  const serviceOptions = createServiceOptions(
    parsed.options,
    rootPath.absolutePath,
  );
  const zvecGrep = await createZvecGrep(serviceOptions);
  const progress = createIndexProgressReporter({ color: parsed.options.color });
  try {
    const infoBefore = await zvecGrep.info({
      root: rootPath.absolutePath,
      includeStatus: parsed.options.rebuild !== true,
    });
    const schema = resolveAuthorizationSchema(
      configuredEmbeddingReference(
        parsed.options,
        infoBefore.workspaceIndex?.embedding,
      ),
      infoBefore.workspaceIndex?.embedding,
    );
    assertEmbeddingModelCompatible(
      infoBefore.workspaceIndex?.embedding,
      schema,
      parsed.options.rebuild === true,
    );
    const workspaceRuntime = workspaceRuntimeFromInfo(infoBefore);
    assertRequestedEndpointCompatible(
      infoBefore,
      workspaceRuntime,
      parsed.options.endpoint,
      parsed.options.rebuild === true,
    );
    const modelInfo =
      schema?.provider === "qwen"
        ? await embeddingModelInfo(schema, serviceOptions, workspaceRuntime)
        : undefined;
    const plan = modelInfo
      ? await planRemoteIndexAuthorization({
          info: infoBefore,
          model: modelInfo,
          rebuild: parsed.options.rebuild,
          store: authorizationStore(parsed.options),
        })
      : undefined;
    const authorizationResolution = plan
      ? await authorizeCliPlan(plan, parsed.options)
      : {};
    printIndexPathFilterTip(parsed.options);
    const result = await withRemoteEmbeddingOperationPermit(
      authorizationResolution.authorization,
      () =>
        zvecGrep.index({
          root: rootPath.absolutePath,
          rootPaths: explicitRoot ? [rootPath] : undefined,
          rebuild: parsed.options.rebuild,
          resetPaths: parsed.options.resetPaths,
          globs: parsed.options.globs,
          insensitiveGlobs: parsed.options.insensitiveGlobs,
          fileTypes: parsed.options.fileTypes,
          excludedFileTypes: parsed.options.excludedFileTypes,
          hidden: parsed.options.hidden,
          noIgnore: parsed.options.noIgnore,
          ignoreFiles: parsed.options.ignoreFiles,
          maxDepth: parsed.options.maxDepth,
          maxFileSizeBytes: parsed.options.maxFileSizeBytes,
          follow: parsed.options.follow,
          embeddingConcurrency: parsed.options.embeddingConcurrency,
          onProgress: progress.report,
        }),
    );
    progress.finish();
    const info = await zvecGrep.info({ root: rootPath.absolutePath });
    printIndexResult(
      "Workspace index",
      result,
      parsed.options,
      info.workspaceIndex?.rootPaths,
    );
  } catch (error) {
    progress.finish();
    throw error;
  } finally {
    await zvecGrep.close();
  }
}

async function runDropIndex(parsed: ParsedArgs, root: string): Promise<void> {
  if (!(await confirmIndexDrop(root, parsed.options.yes === true))) {
    console.log("Index drop cancelled.");
    return;
  }

  const printResult = (removed: boolean): void => {
    console.log(
      removed ? `Dropped index for ${root}` : `No index found for ${root}`,
    );
  };
  await routeByMode({
    mode: resolveClientMode(parsed.options.mode),
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index_drop",
        { root },
      );
      printResult(result.removed === true);
    },
    direct: async () => {
      const zvecGrep = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        printResult(await zvecGrep.dropIndex({ root }));
      } finally {
        await zvecGrep.close();
      }
    },
  });
}

async function confirmIndexDrop(
  root: string,
  accepted: boolean,
): Promise<boolean> {
  if (accepted) {
    return true;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(
      "zg index --drop requires --yes in a non-interactive shell",
    );
  }

  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  try {
    const answer = await readline.question(
      `Drop the index for ${root}? [y/N] `,
    );
    return ["y", "yes"].includes(answer.trim().toLowerCase());
  } finally {
    readline.close();
  }
}

async function runServer(parsed: ParsedArgs): Promise<void> {
  if (parsed.positionals.length > 0) {
    throw new Error(
      `zg server ${parsed.options.serverStdio ? "--stdio" : parsed.options.serverAction} does not accept positional arguments`,
    );
  }
  const { readPackageVersion } = await import("./version.js");
  if (parsed.options.serverStdio) {
    const { runStdioBootstrapBridge } = await import("../mcp/stdio-bridge.js");
    await runStdioBootstrapBridge({
      cliPath: process.argv[1]!,
      version: readPackageVersion(),
      home: parsed.options.home,
      tokenFile: parsed.options.serverTokenFile,
      listen: parsed.options.listen,
      mcpToolset: parsed.options.mcpToolset,
    });
    return;
  }
  if (parsed.options.serverAction === "on") {
    const { startServer } = await import("../daemon/server-controller.js");
    const status = await startServer({
      cliPath: process.argv[1]!,
      listen: parsed.options.listen,
      tokenFile: parsed.options.serverTokenFile,
      home: parsed.options.home,
      mcpToolset: parsed.options.mcpToolset,
    });
    printServerControlStatus(status);
    return;
  }
  if (parsed.options.serverAction === "off") {
    const { stopServer } = await import("../daemon/server-controller.js");
    printServerControlStatus(
      await stopServer(
        parsed.options.home,
        30_000,
        parsed.options.serverTokenFile,
      ),
    );
    return;
  }
  if (parsed.options.serverAction === "status") {
    const status = await serverStatus(parsed.options.home);
    printServerControlStatus(status);
    if (parsed.options.checkReady && !status.ready) {
      throw new Error("zvec-grep server is not ready");
    }
    return;
  }

  const { runDaemonForeground } = await import("../daemon/runtime.js");
  await runDaemonForeground({
    version: readPackageVersion(),
    listen: parsed.options.listen,
    tokenFile: parsed.options.serverTokenFile,
    home: parsed.options.home,
    mcpToolset: parsed.options.mcpToolset,
    serviceOptions: createServiceOptions(parsed.options, process.cwd()),
  });
}

async function runStatus(parsed: ParsedArgs): Promise<void> {
  const root = parsed.positionals[0] ?? process.cwd();
  if (parsed.positionals.length > 1) {
    throw new Error("zg status accepts at most one root path");
  }

  const mode = resolveClientMode(parsed.options.mode);
  const state = await routeByMode({
    mode,
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index_status",
        { root: resolve(root) },
      );
      return printServerIndexInfo(
        result as Parameters<typeof printServerIndexInfo>[0],
        parsed.options,
      );
    },
    direct: async () => {
      const zvecGrep = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        return printWorkspaceInfo(
          await zvecGrep.info({ root }),
          parsed.options,
        );
      } finally {
        await zvecGrep.close();
      }
    },
  });
  if (parsed.options.checkReady && state !== "ready") {
    throw new Error(`Workspace index is not ready (state: ${state})`);
  }
}

async function runExplore(parsed: ParsedArgs): Promise<void> {
  const query = parsed.positionals[0]!;
  const root = resolve(process.cwd());
  const input = {
    root,
    query,
    seedId: parsed.options.seedId,
    searchLimit: parsed.options.limit,
    traversalDepth: parsed.options.depth,
    maxFiles: parsed.options.maxFiles,
  };
  await routeByMode({
    mode: resolveClientMode(parsed.options.mode),
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      console.log(
        await daemonClient(parsed.options).callTextTool("zvec_grep_explore", {
          root,
          query,
          seedId: input.seedId,
          limit: input.searchLimit,
          depth: input.traversalDepth,
          maxFiles: input.maxFiles,
        }),
      );
    },
    direct: async () => {
      const service = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        printExploreResult(await service.explore(input));
      } finally {
        await service.close();
      }
    },
  });
}

async function runGraphNeighborhood(parsed: ParsedArgs): Promise<void> {
  const direction = parsed.command;
  if (
    direction !== "callers" &&
    direction !== "callees" &&
    direction !== "impact"
  ) {
    throw new Error(`unexpected graph command: ${parsed.command}`);
  }
  const root = resolve(process.cwd());
  const input = {
    root,
    direction,
    query: parsed.positionals[0]!,
    depth: parsed.options.depth,
    limit: parsed.options.limit,
    seedId: parsed.options.seedId,
  };
  await routeByMode({
    mode: resolveClientMode(parsed.options.mode),
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      console.log(
        await daemonClient(parsed.options).callTextTool(
          `zvec_grep_${direction}`,
          input,
        ),
      );
    },
    direct: async () => {
      const service = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        printNeighborhoodResult(await service.graphNeighborhood(input));
      } finally {
        await service.close();
      }
    },
  });
}

async function runQuery(parsed: ParsedArgs): Promise<void> {
  const rgInput = parsed.options.rg
    ? normalizeManagedRgInput(parsed)
    : undefined;
  const commandOptions = rgInput?.options ?? parsed.options;
  const queries = (
    rgInput?.queries ?? [
      ...parsed.positionals,
      ...(parsed.options.hybridQueries ?? []),
    ]
  )
    .map((query) => query.trim())
    .filter((query) => query.length > 0);
  const routes = parsed.options.routes ?? [];
  if (
    queries.length === 0 &&
    routes.length === 0 &&
    (parsed.options.rgOptions?.patternFiles?.length ?? 0) === 0
  ) {
    throw new Error(
      parsed.options.rg
        ? "zg query --rg requires a pattern. Use zg help query for examples."
        : "zg query requires text or --hybrid/--fts/--vector routes. Use zg help query for examples.",
    );
  }
  if (!commandOptions.rg) {
    const mode = resolveClientMode(commandOptions.mode);
    if (mode !== "direct") {
      await routeByMode({
        mode,
        serverAvailable: () => daemonIsReady(commandOptions.home),
        server: () => runServerQuery(commandOptions, queries, routes),
        direct: () => runDirectQuery(commandOptions, queries),
      });
      return;
    }
  }
  await runDirectQuery(commandOptions, queries);
}

async function runDirectQuery(
  commandOptions: CliOptions,
  queries: readonly string[],
): Promise<void> {
  if (commandOptions.refresh === "background") {
    console.error(
      "warning: --refresh background requires Server mode; Direct mode uses --refresh off",
    );
  }
  const serviceOptions = createServiceOptions(commandOptions, undefined);
  const zvecGrep = await createZvecGrep(serviceOptions);
  const progress = createIndexProgressReporter({
    color: commandOptions.color,
  });
  try {
    const contextRequest = contextOptions(
      commandOptions,
      queries,
      (progressEvent) => {
        if (progressEvent.phase !== "done") progress.report(progressEvent);
      },
    );
    const info = await directQueryInfo(zvecGrep);
    const schema = info.workspaceIndex?.embedding;
    const workspaceRuntime = workspaceRuntimeFromInfo(info);
    const modelInfo =
      !commandOptions.rg && schema?.provider === "qwen"
        ? await embeddingModelInfo(
            schema,
            createServiceOptions(commandOptions, info.root),
            workspaceRuntime,
          )
        : undefined;
    const plan = modelInfo
      ? await planRemoteSearchAuthorization({
          info,
          model: modelInfo,
          search: normalizedDirectSearchInput(
            commandOptions,
            queries,
            info.root,
          ),
          store: authorizationStore(commandOptions),
        })
      : undefined;
    const authorizationResolution = plan
      ? await authorizeCliPlan(plan, commandOptions, "local_search")
      : {};
    const effectiveContextRequest =
      authorizationResolution.alternative === "local_search"
        ? ftsFallbackContextRequest(contextRequest)
        : contextRequest;
    const result = await withRemoteEmbeddingOperationPermit(
      authorizationResolution.authorization,
      () => zvecGrep.context(effectiveContextRequest),
    );
    progress.finish();
    printCliContextResult(result, commandOptions);
    for (const line of contextWarningLines(result)) {
      console.error(line);
    }
    if (
      effectiveContextRequest.autoUpdate !== true &&
      indexStatusNeedsRefresh(info.status)
    ) {
      printStaleIndexStatus("idle", indexCompletionFromStatus(info.status));
    }

    if (commandOptions.debug) {
      printDebug(result, {
        trace: commandOptions.trace === true,
      });
    }
  } catch (error) {
    progress.finish();
    throw error;
  } finally {
    await zvecGrep.close();
  }
}

async function runServerQuery(
  options: CliOptions,
  queries: readonly string[],
  routes: readonly ZvecGrepContextRoute[],
): Promise<void> {
  const searchPolicy = resolveServerSearchPolicy(options);
  const structuredContent = await daemonClient(options).callTool(
    "zvec_grep_search",
    {
      root: resolve(process.cwd()),
      apiKey: options.apiKey,
      device: options.device,
      queries: queries.length ? queries : undefined,
      routes: routes.length ? routes : undefined,
      fuse: options.fuse,
      limit: options.limit,
      trace: options.trace,
      preferSymbol: options.preferSymbol,
      symbolTypes: options.symbolTypes,
      globs: options.globs,
      insensitiveGlobs: options.insensitiveGlobs,
      fileTypes: options.fileTypes,
      excludedFileTypes: options.excludedFileTypes,
      hidden: options.hidden,
      noIgnore: options.noIgnore,
      ignoreFiles: options.ignoreFiles,
      maxDepth: options.maxDepth,
      maxFileSizeBytes: options.maxFileSizeBytes,
      follow: options.follow,
      embeddingConcurrency: options.embeddingConcurrency,
      modifiedAfter: options.modifiedAfter,
      modifiedBefore: options.modifiedBefore,
      freshness: searchPolicy.freshness,
      autoUpdate: searchPolicy.autoUpdate,
    },
    {
      toolContract: {
        inputProperties: ["routes"],
        outputProperties: ["result"],
        errorMessage: INCOMPATIBLE_SERVER_SEARCH_MESSAGE,
      },
    },
  );
  const response = parseServerSearchResponse(structuredContent);
  printCliContextResult(response.result, options);
  for (const line of contextWarningLines(response.result)) {
    console.error(line);
  }
  if (response.freshness === "possibly_stale") {
    printStaleIndexStatus(response.indexing?.state, response.indexing);
  }
  if (options.debug) {
    printDebug(response.result, { trace: options.trace === true });
  }
}

function printStaleIndexStatus(
  state: string | undefined,
  completion: { completed?: number; total?: number } | undefined,
): void {
  console.error("status: possibly_stale");
  if (!state) return;
  const progress =
    completion?.completed === undefined || completion.total === undefined
      ? ""
      : ` (${completion.completed}/${completion.total})`;
  console.error("results: served_from_current_index");
  console.error(`background_refresh: ${state}${progress}`);
}

function daemonClient(options: CliOptions): DaemonClient {
  return new DaemonClient({
    serverUrl: resolveServerUrl(),
    home: options.home,
    tokenFile: options.serverTokenFile,
    allowRemote: options.allowRemote,
  });
}

function ftsFallbackContextRequest(
  request: ZvecGrepContextOptions,
): ZvecGrepContextOptions {
  const queries = [
    ...(request.queries ?? []),
    ...(request.query ? [request.query] : []),
    ...(request.routes ?? []).map((route) => route.query),
  ];
  return {
    ...request,
    query: undefined,
    queries: undefined,
    routes: queries.map((query) => ({ mode: "fts", query })),
    autoUpdate: false,
  };
}

async function directQueryInfo(service: ZvecGrep): Promise<ZvecGrepInfoResult> {
  return await service.info({ root: process.cwd() });
}

function normalizedDirectSearchInput(
  options: CliOptions,
  queries: readonly string[],
  root: string,
): NormalizedSearchInput {
  const policy = resolveDirectSearchPolicy(options);
  return {
    root,
    apiKey: options.apiKey,
    device: options.device,
    queries: !options.rg && queries.length > 0 ? [...queries] : undefined,
    routes: [...(options.routes ?? [])],
    ...policy,
  };
}

async function daemonIsReady(home?: string): Promise<boolean> {
  return (await serverStatus(home)).ready;
}

function printServerControlStatus(
  status: Awaited<ReturnType<typeof serverStatus>>,
): void {
  console.log(
    `Server: ${status.running ? (status.ready ? "ready" : "starting") : "stopped"}`,
  );
  if (status.pid) console.log(`PID: ${status.pid}`);
  if (status.serverUrl) console.log(`URL: ${status.serverUrl}`);
  if (status.mcpToolset) console.log(`MCP toolset: ${status.mcpToolset}`);
}

function contextOptions(
  options: CliOptions,
  queries: readonly string[],
  onAutoUpdateProgress?: (progress: IndexProgress) => void,
): ZvecGrepContextOptions {
  const policy = resolveDirectSearchPolicy(options);
  return {
    queries: queries.length > 0 ? queries : undefined,
    rg: options.rg,
    rgOptions: options.rgOptions,
    rgPaths: options.rgPaths,
    routes: options.routes,
    fuse: options.fuse,
    limit: options.limit,
    autoUpdate: policy.autoUpdate,
    onAutoUpdateProgress,
    trace: options.trace,
    preferSymbol: options.preferSymbol,
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
    follow: options.follow,
    modifiedAfter: options.modifiedAfter,
    modifiedBefore: options.modifiedBefore,
    symbolTypes: options.symbolTypes,
    embeddingConcurrency: options.embeddingConcurrency,
  };
}

export function createServiceOptions(
  options: CliOptions,
  root: string | undefined,
): CreateZvecGrepOptions {
  return {
    root,
    home: options.home ?? process.env.ZVEC_GREP_HOME,
    embedding: options.embedding,
    apiKey: options.apiKey,
    endpoint: options.endpoint,
    modelCacheDir: options.modelCacheDir,
    device: options.device,
    authorizationSigningKeyPath: process.env.ZVEC_GREP_AUTHORIZATION_KEY_FILE,
  };
}

function resolveIndexRoot(root: string | undefined): string {
  if (root !== undefined) {
    return root;
  }

  return findNearestWorkspace(process.cwd())?.root ?? process.cwd();
}

function indexRootPath(path: string, options: CliOptions): RootPath {
  return {
    absolutePath: resolve(path),
    recursive: true,
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
    follow: options.follow,
  };
}
