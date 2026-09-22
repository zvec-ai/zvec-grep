import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
import {
  appendFile,
  mkdir,
  readFile,
  readdir,
  lstat,
  writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { join, resolve, delimiter, extname } from "node:path";
import { platform, arch, cpus } from "node:os";
import { parseArgs } from "node:util";
import { performance } from "node:perf_hooks";
import {
  loadSuite,
  run,
  prepareCorpus,
  validateGoldSources,
  corpusManifest,
  modelArtifactManifest,
  fileHash,
  readJson,
  writeJson,
  repositorySlug,
  inside,
} from "../../core/lib.mjs";
import { aggregate } from "./report.mjs";
import { NativeIndexProductError, snapshotIndex } from "./snapshot.mjs";

const MCP_CLIENT_PACKAGE = "@modelcontextprotocol/client@2.0.0";

export function requestArguments(task, root, mode, protocol) {
  assert.ok(protocol.modes.includes(mode), `unknown mode: ${mode}`);
  assert.equal(
    protocol.preview,
    "mcp-default",
    "the Rust benchmark requires the public MCP default presentation",
  );
  return {
    root,
    ...(mode === "hybrid" ? { query: task.query } : { [mode]: [task.query] }),
    limit: protocol.limit,
    ...protocol.request,
  };
}

export function callPlan(tasks, protocol) {
  assert.deepEqual(protocol.modes, ["hybrid", "fts", "vector"]);
  assert.equal(protocol.preview, "mcp-default");
  // Each mode receives every original question five times in the same session.
  // Fixed mode order is part of the protocol, not a controlled speed comparison.
  return protocol.modes.flatMap((mode) =>
    tasks.flatMap((task) =>
      Array.from({ length: protocol.repetitions }, (_, index) => ({
        task,
        mode,
        preview: protocol.preview,
        repetition: index + 1,
        quality_observation: index + 1 === protocol.quality_repetition,
      })),
    ),
  );
}

export function indexSelectionArguments(protocol) {
  const selection = protocol.index_selection;
  assert.equal(selection.content, "code");
  assert.ok(selection.code_extensions.length > 0);
  const args = ["--max-filesize", String(selection.max_file_size_bytes)];
  // Small brace groups stay below the public argument-size limits while covering
  // precisely the frozen CODE extension set. Native ignore rules still apply.
  for (let index = 0; index < selection.code_extensions.length; index += 32) {
    const extensions = selection.code_extensions.slice(index, index + 32);
    extensions.forEach((extension) =>
      assert.match(extension, /^\.[a-z0-9_-]+$/),
    );
    args.push("--iglob", `*{${extensions.join(",")}}`);
  }
  return args;
}

export function auditIndexSelection(files, protocol) {
  const selection = protocol.index_selection;
  const extensions = new Set(selection.code_extensions);
  assert.ok(files.length > 0, "code-only index contains no files");
  for (const file of files) {
    assert.ok(
      extensions.has(extname(file.relativePath).toLowerCase()),
      `non-code extension entered index: ${file.relativePath}`,
    );
    assert.ok(
      Number.isFinite(file.sizeBytes) &&
        file.sizeBytes <= selection.max_file_size_bytes,
      `oversized file entered index: ${file.relativePath}`,
    );
  }
  return {
    content: "code",
    max_file_size_bytes: selection.max_file_size_bytes,
    indexed_files: files.length,
    verified: true,
  };
}

async function freePort() {
  const server = createServer();
  await new Promise((res, rej) => {
    server.once("error", rej);
    server.listen(0, "127.0.0.1", res);
  });
  const port = server.address().port;
  await new Promise((res, rej) =>
    server.close((error) => (error ? rej(error) : res())),
  );
  return port;
}

export function nativeCandidate(packageRoot, consumer, metadata) {
  const bin =
    typeof metadata.bin === "string" ? metadata.bin : metadata.bin?.zg;
  assert.equal(
    typeof bin,
    "string",
    "candidate package must expose the zg binary",
  );
  const cli = resolve(packageRoot, bin);
  assert.ok(
    inside(packageRoot, cli),
    "candidate binary escapes the installed package",
  );
  assert.ok(
    !/\.[cm]?js$/i.test(cli),
    "benchmark candidate must be the native Rust CLI",
  );
  return { consumer, packageRoot, cli, runtime: "rust-native" };
}

async function packageCandidate(packagePath, output) {
  let tarball = resolve(packagePath);
  if ((await lstat(tarball)).isDirectory()) {
    const names = (await readdir(tarball)).filter((name) =>
      name.endsWith(".tgz"),
    );
    assert.equal(
      names.length,
      1,
      "provide a directory containing exactly one npm tarball",
    );
    tarball = join(tarball, names[0]);
  }
  const consumer = join(output, "consumer");
  await mkdir(consumer, { recursive: true });
  await writeJson(join(consumer, "package.json"), {
    private: true,
    type: "module",
  });
  console.log("Installing the packed candidate in an isolated consumer...");
  const installed = await run(
    "npm",
    ["install", "--no-audit", "--no-fund", tarball, MCP_CLIENT_PACKAGE],
    { cwd: consumer },
  );
  await writeJson(join(output, "package-install.json"), installed);
  const packageRoot = join(consumer, "node_modules/@zvec/zvec-grep");
  const metadata = await readJson(join(packageRoot, "package.json"));
  await writeFile(
    join(output, "consumer-lock.json"),
    await readFile(join(consumer, "package-lock.json")),
  );
  const candidate = nativeCandidate(packageRoot, consumer, metadata);
  assert.ok(
    (await lstat(candidate.cli)).isFile(),
    "installed Rust CLI is missing",
  );
  return {
    ...candidate,
    identity: {
      name: metadata.name,
      version: metadata.version,
      runtime: candidate.runtime,
      tarball_sha256: await fileHash(tarball),
      consumer_lock_sha256: await fileHash(join(consumer, "package-lock.json")),
    },
  };
}

function runCandidate(candidate, args, options) {
  return run(candidate.cli, args, options);
}

function referencedSchema(root, reference) {
  if (!reference.startsWith("#/")) return null;
  return reference
    .slice(2)
    .split("/")
    .map((part) => part.replaceAll("~1", "/").replaceAll("~0", "~"))
    .reduce((value, part) => value?.[part], root);
}

export function findSchemaVariant(schema, root, predicate, seen = new Set()) {
  if (!schema || typeof schema !== "object" || seen.has(schema)) return null;
  seen.add(schema);
  if (predicate(schema)) return schema;
  const referenced = schema.$ref && referencedSchema(root, schema.$ref);
  if (referenced) {
    const match = findSchemaVariant(referenced, root, predicate, seen);
    if (match) return match;
  }
  for (const field of ["anyOf", "oneOf", "allOf"])
    for (const variant of schema[field] ?? []) {
      const match = findSchemaVariant(variant, root, predicate, seen);
      if (match) return match;
    }
  return null;
}

export function schemaAllowsType(schema, type) {
  return Array.isArray(schema?.type)
    ? schema.type.includes(type)
    : schema?.type === type;
}

export function isProductPreparationFailure(phase, error) {
  return (
    ["installation", "index", "mcp"].includes(phase) ||
    (phase === "snapshot" &&
      (Boolean(error?.result) || error instanceof NativeIndexProductError))
  );
}

async function runRepository({ suite, repo, tasks, candidate, options }) {
  const { protocol, gold, identity } = suite;
  const modes = protocol.modes;
  const output = join(options.output, repositorySlug(repo.repository));
  await mkdir(join(output, "raw"), { recursive: true });
  const planned = callPlan(tasks, protocol);
  const manifest = {
    schema_version: 1,
    run_id: options.runId,
    repository: repo.repository,
    repository_commit: repo.commit,
    started_at: new Date().toISOString(),
    suite: identity,
    protocol,
    package: candidate.identity,
    candidate_commit: options.candidateCommit,
    environment: {
      platform: platform(),
      architecture: arch(),
      cpus: cpus().length,
      cpu_model: cpus()[0]?.model,
    },
    tasks: tasks.map((task) => task.task_id),
    modes,
    preview: protocol.preview,
    planned_calls: planned.length,
    invalid_reasons: [],
    preparation_status: "pending",
    stage_availability: "pending",
  };
  await writeJson(join(output, "run.json"), manifest);
  let root, env, client, transport, corpusBefore, modelBefore, before;
  let productFailure,
    phase = "corpus";
  const responses = [];
  try {
    root = await prepareCorpus(repo, options.corpus);
    manifest.corpus_root = root;
    manifest.root_mapping =
      "locked repository checkout at the recorded absolute root; historical /app is not replayed";
    assert.ok(
      !inside(root, options.output) && !inside(root, options.modelCache),
      "artifacts/model cache must be outside the search root",
    );
    await validateGoldSources(root, tasks, gold);
    corpusBefore = await corpusManifest(root);
    manifest.corpus_sha256 = corpusBefore.sha256;
    await writeJson(join(output, "corpus.json"), corpusBefore);
    try {
      await lstat(join(root, ".zvec-grep"));
      throw new Error(
        `fresh-index protocol requires a new corpus checkout: ${root}`,
      );
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    const home = join(output, "runtime-home");
    const opencode = join(output, "installation/opencode.json");
    const port = await freePort();
    await mkdir(home, { recursive: true });
    await writeJson(join(home, ".zvec-grep/config.json"), {
      version: 1,
      server: { host: "127.0.0.1", port },
      defaults: {
        embedding: protocol.model,
        modelCacheDir: options.modelCache,
      },
      models: { [protocol.model]: { device: protocol.device } },
    });
    env = {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      OPENCODE_CONFIG: opencode,
      ZVEC_GREP_HOME: join(home, ".zvec-grep"),
      ZVEC_GREP_MODEL_CACHE: options.modelCache,
      ZVEC_GREP_DEVICE: protocol.device,
      NO_COLOR: "1",
      FORCE_COLOR: "0",
      PATH: `${join(candidate.consumer, "node_modules/.bin")}${delimiter}${process.env.PATH ?? ""}`,
    };
    // No custom guidance or proxy. Consume exactly the command generated by zg --install.
    phase = "installation";
    const install = await runCandidate(
      candidate,
      [
        "--install",
        "--target",
        "opencode",
        "--yes",
        "--mcp-transport",
        "stdio",
      ],
      { env, cwd: root },
    );
    await writeJson(join(output, "installation/install.json"), install);
    const config = await readJson(opencode);
    assert.equal(config.mcp?.zvec_grep?.type, "local");
    const command = config.mcp.zvec_grep.command;
    assert.ok(
      Array.isArray(command) &&
        command.length >= 3 &&
        command.every((part) => typeof part === "string"),
    );
    manifest.installation = {
      generated_config: "installation/opencode.json",
      command,
    };
    // Installation may start the daemon. Close it before the independent index build.
    await runCandidate(candidate, ["--server", "off"], {
      env,
      cwd: root,
    });
    phase = "index";
    console.log(`${repo.repository}: building a fresh index`);
    const started = performance.now();
    try {
      const indexed = await runCandidate(
        candidate,
        [
          "--index",
          root,
          "--mode",
          "direct",
          "--embedding",
          protocol.model,
          "--model-cache",
          options.modelCache,
          "--device",
          protocol.device,
          ...indexSelectionArguments(protocol),
          "--debug",
        ],
        { env, cwd: root, timeout: 2_400_000 },
      );
      manifest.index_seconds = (performance.now() - started) / 1000;
      manifest.index_timing_scope =
        "CLI process startup, model load/download if needed, complete index build";
      await writeJson(join(output, "index.json"), indexed);
    } catch (error) {
      manifest.index_seconds = (performance.now() - started) / 1000;
      await writeJson(
        join(output, "index.json"),
        error.result ?? { error: error.message },
      );
      throw error;
    }
    phase = "snapshot";
    await snapshotIndex({
      cli: candidate.cli,
      root,
      output: join(output, "stages/before"),
      env,
    });
    before = await readJson(join(output, "stages/before/summary.json"));
    manifest.index_selection_audit = {
      content: "code",
      max_file_size_bytes: protocol.index_selection.max_file_size_bytes,
      requested_extensions: protocol.index_selection.code_extensions.length,
      verified: "request_arguments_and_public_status",
    };
    manifest.stage_availability = before.stages;
    modelBefore = await modelArtifactManifest(options.modelCache);
    assert.ok(modelBefore.entries.length > 0, "no model artifacts recorded");
    await writeJson(join(output, "model-files.json"), modelBefore);
    manifest.model_files_sha256 = modelBefore.sha256;
    manifest.index_content_sha256 = before.logical_content_sha256;
    manifest.failed_index_files = before.failed_files;
    // Failed extraction is reported, not hidden by omitting files from the corpus.
    manifest.preparation_status = "ready";
    phase = "mcp";
    const require = createRequire(join(candidate.consumer, "package.json"));
    const { Client } = await import(
      pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
    );
    const { StdioClientTransport } = await import(
      pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio")).href
    );
    client = new Client({ name: "zg-retrieval-only", version: "1.0.0" });
    transport = new StdioClientTransport({
      command: command[0],
      args: command.slice(1),
      cwd: root,
      env,
      stderr: "pipe",
    });
    let mcpStderr = "";
    const connectStart = performance.now();
    await client.connect(transport, { timeout: 120_000 });
    transport.stderr?.on("data", (part) => {
      mcpStderr += part;
    });
    manifest.mcp_connect_ms = performance.now() - connectStart;
    const tools = await client.listTools();
    phase = "mcp_contract";
    const search = tools.tools.find((tool) => tool.name === "zvec_grep_search");
    assert.ok(search, "candidate lacks the required public search endpoint");
    const schema = search.inputSchema;
    const fields = schema?.properties;
    for (const field of [
      "root",
      "query",
      "limit",
      "autoUpdate",
      "preferSymbol",
      "freshness",
      ...modes.filter((mode) => mode !== "hybrid"),
    ]) {
      assert.ok(fields?.[field], `candidate search schema lacks ${field}`);
    }
    const typed = (field, type) =>
      findSchemaVariant(fields[field], schema, (entry) =>
        schemaAllowsType(entry, type),
      );
    assert.ok(typed("root", "string"));
    const queryString = typed("query", "string");
    assert.ok(queryString);
    assert.ok(typed("autoUpdate", "boolean"));
    assert.ok(typed("preferSymbol", "boolean"));
    const limitNumber = findSchemaVariant(fields.limit, schema, (entry) =>
      ["number", "integer"].some((type) => schemaAllowsType(entry, type)),
    );
    assert.ok(limitNumber);
    assert.ok(
      findSchemaVariant(
        fields.freshness,
        schema,
        (entry) =>
          (Array.isArray(entry.enum) &&
            entry.enum.includes(protocol.request.freshness)) ||
          entry.const === protocol.request.freshness,
      ),
    );
    assert.ok(
      (limitNumber.minimum ?? 1) <= protocol.limit &&
        (limitNumber.maximum ?? Infinity) >= protocol.limit,
    );
    for (const mode of modes) {
      if (mode !== "hybrid") {
        const array = findSchemaVariant(fields[mode], schema, (entry) =>
          schemaAllowsType(entry, "array"),
        );
        assert.ok(array, `candidate ${mode} route does not accept an array`);
        const item = findSchemaVariant(array.items, schema, (entry) =>
          schemaAllowsType(entry, "string"),
        );
        assert.ok(item, `candidate ${mode} route does not accept strings`);
        assert.ok(
          tasks.every(
            (task) => task.query.length <= (item.maxLength ?? Infinity),
          ),
          "a frozen question exceeds the candidate request limit",
        );
      }
      const args = requestArguments(tasks[0], root, mode, protocol);
      assert.ok(
        !Object.hasOwn(args, "preview"),
        "Rust MCP default presentation must not be overridden",
      );
      assert.ok(
        (search.inputSchema.required ?? []).every((field) => field in args),
        "candidate requires an unsupported request field",
      );
    }
    assert.ok(
      tasks.every(
        (task) => task.query.length <= (queryString.maxLength ?? Infinity),
      ),
      "a frozen question exceeds the candidate request limit",
    );
    await writeJson(join(output, "installation/tools.json"), tools);
    phase = "replay";
    for (const [index, call] of planned.entries()) {
      const args = requestArguments(call.task, root, call.mode, protocol);
      const start = performance.now();
      let response, error;
      try {
        response = await client.callTool(
          { name: "zvec_grep_search", arguments: args },
          undefined,
          { timeout: 120_000 },
        );
      } catch (failure) {
        error = failure.message;
        response = { isError: true, content: [{ type: "text", text: error }] };
      }
      const record = {
        task_id: call.task.task_id,
        mode: call.mode,
        preview: call.preview,
        repetition: call.repetition,
        quality_observation: call.quality_observation,
        session_first_query: index === 0,
        latency_ms: performance.now() - start,
        request: { name: "zvec_grep_search", arguments: args },
        transport_error: error ?? null,
        raw_path: `raw/${call.task.task_slug}-${call.mode}-${call.preview}-${call.repetition}.json`,
      };
      await writeJson(join(output, record.raw_path), response);
      record.raw_sha256 = await fileHash(join(output, record.raw_path));
      await appendFile(
        join(output, "requests.jsonl"),
        `${JSON.stringify(record)}\n`,
      );
      responses.push(record);
    }
    await writeFile(join(output, "mcp-stderr.log"), mcpStderr);
  } catch (error) {
    manifest.preparation_status = `${phase}_failed`;
    if (isProductPreparationFailure(phase, error))
      productFailure = error.message;
    else manifest.invalid_reasons.push(`${phase}: ${error.message}`);
    console.error(`${repo.repository}: ${phase}: ${error.message}`);
  } finally {
    if (client) await client.close().catch(() => undefined);
    if (env) {
      try {
        await runCandidate(candidate, ["--server", "off"], {
          env,
          cwd: root,
          timeout: 60_000,
        });
      } catch (error) {
        manifest.invalid_reasons.push(`daemon shutdown: ${error.message}`);
      }
    }
    if (before && root) {
      try {
        await snapshotIndex({
          cli: candidate.cli,
          root,
          output: join(output, "stages/after"),
          env,
        });
        const after = await readJson(join(output, "stages/after/summary.json"));
        assert.equal(
          after.logical_content_sha256,
          before.logical_content_sha256,
          "index content changed during fixed-query replay",
        );
        const corpusAfter = await corpusManifest(root);
        const modelAfter = await modelArtifactManifest(options.modelCache);
        await writeJson(join(output, "corpus-after.json"), corpusAfter);
        await writeJson(join(output, "model-files-after.json"), modelAfter);
        assert.equal(
          corpusAfter.sha256,
          corpusBefore.sha256,
          "corpus content changed",
        );
        assert.equal(
          (await run("git", ["-C", root, "rev-parse", "HEAD"])).stdout.trim(),
          repo.commit,
          "corpus commit changed",
        );
        assert.equal(
          (
            await run("git", [
              "-C",
              root,
              "status",
              "--porcelain",
              "--untracked-files=all",
              "--",
              ".",
              ":(exclude).zvec-grep",
            ])
          ).stdout,
          "",
          "corpus gained modifications or untracked files during replay",
        );
        assert.equal(
          modelAfter.sha256,
          modelBefore.sha256,
          "model artifacts changed during replay",
        );
        manifest.post_run_integrity = "verified";
      } catch (error) {
        manifest.invalid_reasons.push(error.message);
      }
    }
    for (const call of planned.slice(responses.length)) {
      const record = {
        task_id: call.task.task_id,
        mode: call.mode,
        preview: call.preview,
        repetition: call.repetition,
        quality_observation: call.quality_observation,
        session_first_query: false,
        latency_ms: null,
        request: {
          name: "zvec_grep_search",
          arguments: requestArguments(
            call.task,
            root ?? "<unavailable>",
            call.mode,
            protocol,
          ),
        },
        preparation_error: productFailure ?? null,
        harness_error: productFailure
          ? null
          : manifest.invalid_reasons.join("; "),
        raw_path: `raw/${call.task.task_slug}-${call.mode}-${call.preview}-${call.repetition}.json`,
      };
      await writeJson(join(output, record.raw_path), {
        isError: true,
        content: [
          { type: "text", text: productFailure ?? record.harness_error },
        ],
      });
      record.raw_sha256 = await fileHash(join(output, record.raw_path));
      await appendFile(
        join(output, "requests.jsonl"),
        `${JSON.stringify(record)}\n`,
      );
    }
    manifest.finished_at = new Date().toISOString();
    manifest.preparation_error = productFailure ?? null;
    await writeJson(join(output, "run.json"), manifest);
  }
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: {
      package: { type: "string" },
      output: { type: "string" },
      corpus: { type: "string" },
      repository: { type: "string" },
      tasks: { type: "string" },
      "model-cache": { type: "string" },
      "candidate-commit": { type: "string", default: "unrecorded" },
    },
  });
  assert.ok(
    values.package && values.output && values.corpus,
    "usage: node run.mjs --package candidate.tgz --output NEW_DIR --corpus CORPUS_DIR [--repository owner/name]",
  );
  const suite = await loadSuite();
  const selected = values.tasks?.split(",");
  if (selected)
    selected.forEach((id) =>
      assert.ok(
        suite.lock.tasks.some((task) => task.task_id === id),
        `unknown task: ${id}`,
      ),
    );
  const tasks = suite.lock.tasks.filter(
    (task) =>
      (!values.repository || task.repository === values.repository) &&
      (!selected || selected.includes(task.task_id)),
  );
  assert.ok(tasks.length, "empty task selection");
  const options = {
    output: resolve(values.output),
    corpus: resolve(values.corpus),
    modelCache: resolve(
      values["model-cache"] ?? join(values.output, "model-cache"),
    ),
    candidateCommit: values["candidate-commit"],
    runId: new Date().toISOString(),
  };
  await mkdir(options.output, { recursive: false }); // Never overwrite a prior run.
  await mkdir(options.corpus, { recursive: true });
  await mkdir(options.modelCache, { recursive: true });
  const candidate = await packageCandidate(values.package, options.output);
  for (const repo of suite.lock.repositories.filter((repo) =>
    tasks.some((task) => task.repository === repo.repository),
  )) {
    await runRepository({
      suite,
      repo,
      tasks: tasks.filter((task) => task.repository === repo.repository),
      candidate,
      options,
    });
  }
  const report = await aggregate(options.output, {
    expectedTasks: tasks.map((task) => task.task_id),
  });
  console.log(`Report: ${join(options.output, "report.md")}`);
  if (!report.integrity_passed) process.exitCode = 1;
}
