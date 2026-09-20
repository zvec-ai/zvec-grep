import assert from "node:assert/strict";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { TransformersJsEmbeddingModel } from "../../dist/engine/models/backends/transformers-js.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const reference = "local/all-minilm-l6-v2";
const schema = {
  provider: "local",
  model: "all-minilm-l6-v2",
  dimension: 384,
  metric: "cosine",
};

function nextTurn() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function setup(t) {
  const directory = await createTemporaryDirectory(t, "zg-model-lifecycle-");
  const previousHome = process.env.ZVEC_GREP_HOME;
  process.env.ZVEC_GREP_HOME = join(directory, "home");
  t.after(() => {
    if (previousHome === undefined) delete process.env.ZVEC_GREP_HOME;
    else process.env.ZVEC_GREP_HOME = previousHome;
  });
  const service = await createZvecGrep({
    root: directory,
    modelCacheDir: join(directory, "models"),
    device: "cuda",
  });
  t.after(() => service.close());
  const events = [];
  const live = new Set();
  const disposed = new Set();
  const blockedEmbeddings = new Map();
  const blockedDisposals = new Map();
  let peakLive = 0;
  // Exercise real service cache keys and backend construction without loading
  // model artifacts. Deferred native work makes each lifecycle boundary visible.
  t.mock.method(
    TransformersJsEmbeddingModel.prototype,
    "doEmbed",
    async function (contents) {
      assert.equal(
        disposed.has(this),
        false,
        "embedding used a disposed model",
      );
      live.add(this);
      peakLive = Math.max(peakLive, live.size);
      events.push({ kind: "embed", model: this });
      await blockedEmbeddings.get(this);
      events.push({ kind: "embedded", model: this });
      return {
        vectors: contents.map(() => [
          1,
          ...Array(this.info.dimension - 1).fill(0),
        ]),
        truncated: [],
      };
    },
  );
  t.mock.method(
    TransformersJsEmbeddingModel.prototype,
    "dispose",
    async function () {
      assert.equal(disposed.has(this), false, "model disposed more than once");
      events.push({ kind: "disposing", model: this });
      await blockedDisposals.get(this);
      disposed.add(this);
      live.delete(this);
      events.push({ kind: "disposed", model: this });
    },
  );
  return {
    service,
    events,
    live,
    disposed,
    blockedEmbeddings,
    blockedDisposals,
    peakLive: () => peakLive,
    operation: (run) => service.withEmbeddingModelOperation(run),
    indexModel: (concurrency) =>
      service.embeddingModelFromReference(
        reference,
        undefined,
        {},
        concurrency,
      ),
    queryModel: () => service.recoverEmbeddingModel(schema),
    embed: (model, purpose) =>
      model.embed([{ kind: "text", text: "answer" }], { purpose }),
  };
}

test("query model loading waits for index-variant disposal, including callers arriving during disposal", async (t) => {
  const state = await setup(t);
  const index = await state.operation(async () => {
    const model = await state.indexModel(2);
    await state.embed(model, "document");
    return model;
  });
  const disposal = Promise.withResolvers();
  state.blockedDisposals.set(index, disposal.promise);
  const query = () =>
    state.operation(async () => {
      const model = await state.queryModel();
      await state.embed(model, "query");
      return model;
    });
  const first = query();
  await nextTurn();
  assert.equal(state.events.at(-1).kind, "disposing");
  assert.equal(state.events.at(-1).model, index);
  const second = query();
  await nextTurn();
  assert.equal(
    state.events.filter((event) => event.kind === "embed").length,
    1,
  );

  disposal.resolve();
  const [firstModel, secondModel] = await Promise.all([first, second]);
  assert.equal(firstModel, secondModel);
  assert.notEqual(firstModel, index);
  assert.equal(firstModel.info.defaultConcurrency, 1);
  assert.equal(state.disposed.has(index), true);
  assert.equal(state.peakLive(), 1);
});

test("query-refresh-query phase switches dispose each unused concurrency variant before loading the next", async (t) => {
  const state = await setup(t);
  const previousQuery = await state.operation(async () => {
    const model = await state.queryModel();
    await state.embed(model, "query");
    return model;
  });
  const refreshedQuery = await state.operation(async () => {
    const index = await state.indexModel(3);
    assert.equal(state.disposed.has(previousQuery), true);
    await state.embed(index, "document");
    const query = await state.queryModel();
    assert.equal(state.disposed.has(index), true);
    await state.embed(query, "query");
    return query;
  });

  assert.notEqual(refreshedQuery, previousQuery);
  assert.equal(state.live.size, 1);
  assert.equal(state.peakLive(), 1);
  assert.deepEqual(
    state.events
      .filter((event) => event.kind === "embed")
      .map((event) => event.model.info.defaultConcurrency),
    [1, 3, 1],
  );
});

test("Transformers index limit one and automatic query concurrency share the same cached model", async (t) => {
  const state = await setup(t);
  const index = await state.operation(async () => {
    const model = await state.indexModel(1);
    await state.embed(model, "document");
    return model;
  });
  const query = await state.operation(async () => {
    const model = await state.queryModel();
    await state.embed(model, "query");
    return model;
  });
  assert.equal(query, index);
  assert.equal(state.disposed.size, 0);
  assert.equal(state.peakLive(), 1);
});

test("llama only treats automatic concurrency as one when CPU execution is certain", async (t) => {
  const directory = await createTemporaryDirectory(t, "zg-llama-lifecycle-");
  const reference = "local/qwen3-embedding-0.6b";
  const schema = { provider: "local", model: "qwen3-embedding-0.6b" };
  for (const device of ["cpu", "cuda", "auto"]) {
    const service = await createZvecGrep({ root: directory, device });
    t.after(() => service.close());
    const index = await service.withEmbeddingModelOperation(() =>
      service.embeddingModelFromReference(reference, undefined, {}, 1),
    );
    const query = await service.withEmbeddingModelOperation(() =>
      service.recoverEmbeddingModel(schema),
    );
    assert.equal(index === query, device === "cpu", device);
  }
});

test("switching concurrency defers disposal while another operation still uses the old model", async (t) => {
  const state = await setup(t);
  const entered = Promise.withResolvers();
  const embedding = Promise.withResolvers();
  let queryModel;
  const query = state.operation(async () => {
    queryModel = await state.queryModel();
    state.blockedEmbeddings.set(queryModel, embedding.promise);
    entered.resolve();
    await state.embed(queryModel, "query");
  });
  await entered.promise;
  const indexModel = await state.operation(async () => {
    const model = await state.indexModel(2);
    await state.embed(model, "document");
    return model;
  });
  assert.equal(state.disposed.has(queryModel), false);
  assert.equal(
    state.events.some((event) => event.kind === "disposing"),
    false,
  );
  assert.equal(state.live.size, 2);

  embedding.resolve();
  await query;
  assert.equal(state.disposed.has(queryModel), true);
  assert.equal(state.disposed.has(indexModel), false);
  assert.equal(state.live.size, 1);
  assert.equal(
    state.events.filter((event) => event.kind === "disposed").length,
    1,
  );
});

test("concurrent query-index-query calls reuse both active variants and retire the idle older one", async (t) => {
  const state = await setup(t);
  const firstEntered = Promise.withResolvers();
  const indexEntered = Promise.withResolvers();
  const secondEntered = Promise.withResolvers();
  const queryEmbedding = Promise.withResolvers();
  const indexEmbedding = Promise.withResolvers();
  let firstModel;
  let indexModel;
  let secondModel;
  const first = state.operation(async () => {
    firstModel = await state.queryModel();
    state.blockedEmbeddings.set(firstModel, queryEmbedding.promise);
    firstEntered.resolve();
    await state.embed(firstModel, "query");
  });
  await firstEntered.promise;
  const index = state.operation(async () => {
    indexModel = await state.indexModel(2);
    state.blockedEmbeddings.set(indexModel, indexEmbedding.promise);
    indexEntered.resolve();
    await state.embed(indexModel, "document");
  });
  await indexEntered.promise;
  const second = state.operation(async () => {
    secondModel = await state.queryModel();
    secondEntered.resolve();
    await state.embed(secondModel, "query");
  });
  await secondEntered.promise;

  assert.equal(secondModel, firstModel);
  assert.notEqual(indexModel, firstModel);
  assert.equal(state.live.size, 2);
  assert.equal(state.peakLive(), 2);
  assert.equal(state.disposed.size, 0);
  queryEmbedding.resolve();
  await Promise.all([first, second]);
  assert.equal(state.disposed.size, 0);

  indexEmbedding.resolve();
  await index;
  assert.equal(state.disposed.has(indexModel), true);
  assert.equal(state.disposed.has(firstModel), false);
  assert.equal(state.live.size, 1);
});

test("a failed variant disposal reaches its caller without poisoning later model selection or close", async (t) => {
  const state = await setup(t);
  const index = await state.operation(async () => {
    const model = await state.indexModel(2);
    await state.embed(model, "document");
    return model;
  });
  const failure = new Error("pipeline disposal failed");
  const dispose = t.mock.method(index, "dispose", async () => {
    throw failure;
  });

  await assert.rejects(
    state.operation(() => state.queryModel()),
    (error) => error === failure,
  );
  const query = await state.operation(async () => {
    const model = await state.queryModel();
    await state.embed(model, "query");
    return model;
  });
  assert.notEqual(query, index);
  assert.equal(query.info.defaultConcurrency, 1);
  assert.equal(dispose.mock.callCount(), 1);

  await state.service.close();
  assert.equal(state.disposed.has(query), true);
});

test("retired model cleanup attempts every disposal and reports all failures", async (t) => {
  const state = await setup(t);
  const active = Promise.withResolvers();
  const operation = state.operation(() => active.promise);
  const models = await state.operation(async () => {
    const models = [];
    for (const concurrency of [2, 3, 4, 5]) {
      const model = await state.indexModel(concurrency);
      await state.embed(model, "document");
      models.push(model);
    }
    return models;
  });
  const firstFailure = new Error("first retired pipeline failed to dispose");
  const lastFailure = new Error("last retired pipeline failed to dispose");
  // Retirement visits older variants from most to least recently used. A
  // failure must not skip the models still waiting behind it in that batch.
  t.mock.method(models[2], "dispose", async () => {
    throw firstFailure;
  });
  t.mock.method(models[0], "dispose", async () => {
    throw lastFailure;
  });
  const rejected = assert.rejects(operation, (error) => {
    assert.ok(error instanceof AggregateError);
    assert.deepEqual(error.errors, [firstFailure, lastFailure]);
    return true;
  });
  active.resolve();
  await rejected;
  assert.equal(state.disposed.has(models[1]), true);
  assert.equal(state.disposed.has(models[3]), false);

  await state.service.close();
  assert.equal(state.disposed.has(models[3]), true);
});

test("close disposes remaining cached models even when an earlier disposal fails", async (t) => {
  const state = await setup(t);
  const [first, second] = await state.operation(async () => {
    const first = await state.queryModel();
    const second = await state.service.embeddingModelFromReference(
      "local/bge-small-en-v1.5",
    );
    await state.embed(first, "query");
    await state.embed(second, "query");
    return [first, second];
  });
  const failure = new Error("cached pipeline disposal failed");
  t.mock.method(first, "dispose", async () => {
    throw failure;
  });

  await assert.rejects(state.service.close(), (error) => error === failure);
  assert.equal(state.disposed.has(second), true);
});
