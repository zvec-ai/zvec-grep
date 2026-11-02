import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { cpus, totalmem, platform, arch, loadavg } from 'node:os';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { NODE_RELEASE } from '../../shared/retrieval-package.mjs';
import { fileHash, run, writeJson } from '../core/io.mjs';
import { productCandidate, runCandidate } from '../engines/zg/run.mjs';
import { indexConcurrencyArguments } from '../core/performance.mjs';
import { buildCombined } from '../expansion/combined.mjs';
import { compareRuns, markdownVersionComparison } from '../expansion/compare.mjs';
import { performanceReport, validatePerformanceCoverage } from './report.mjs';

const { values } = parseArgs({ options: { output: { type: 'string' } } });
assert.ok(values.output, 'requires --output NEW_DIRECTORY');
const output = resolve(values.output);
const repository = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
await mkdir(output, { recursive: false });
await mkdir(join(output, 'logs'));
const registry = 'https://registry.npmjs.org';
const modelCache = join(output, 'model-cache');
const models = ['local/potion-code-16m-v2', 'local/potion-multilingual-128m'];
const suites = ['zg', 'beir', 'duretrieval', 'quarry'];
const bundles = { node: { reports: {} }, rust: { reports: {} } };
const failures = [];
const metadata = {
  schema_version: 1,
  started_at: new Date().toISOString(),
  harness_commit: (await run('git', ['rev-parse', 'HEAD'], { cwd: repository })).stdout.trim(),
  node_runtime: process.version,
  hardware: { cpu_model: cpus()[0].model, logical_cpus: cpus().length, memory_bytes: totalmem(), platform: platform(), arch: arch() },
};
const hostSamples = [];
let sampling = false;
async function sampleHost() {
  if (sampling) return;
  sampling = true;
  try {
    const result = await run('ps', ['-C', 'Runner.Worker', '-o', 'pid='], { timeout: 5000 }).catch(e => e.result);
    hostSamples.push({ at: new Date().toISOString(), loadavg: loadavg(), runner_workers: result?.stdout.trim().split(/\s+/).filter(Boolean).length ?? null });
  } finally { sampling = false; }
}
await sampleHost();
const samplingTimer = setInterval(() => sampleHost().catch(() => {}), 5000);

async function execute(label, command, args, env = process.env) {
  console.log(`[${new Date().toISOString()}] ${label}`);
  const log = createWriteStream(join(output, 'logs', `${label}.log`));
  const child = spawn(command, args, { cwd: repository, env, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.pipe(log, { end: false });
  child.stderr.pipe(log, { end: false });
  const code = await new Promise((res, rej) => { child.on('error', rej); child.on('close', res); });
  await new Promise(res => log.end(res));
  assert.equal(code, 0, `${label} failed (${code}); inspect logs/${label}.log`);
}

async function install(arm, selector) {
  const response = await fetch(`${registry}/@zvec%2fzvec-grep/${selector}`);
  assert.ok(response.ok, `npm metadata unavailable: ${response.status}`);
  const info = await response.json();
  const prefix = join(output, 'prefixes', arm);
  await execute(`install-${arm}`, 'npm', ['install', '-g', `@zvec/zvec-grep@${selector}`, `--registry=${registry}`, '--prefix', prefix, '--no-audit', '--no-fund']);
  const root = join(prefix, 'lib/node_modules/@zvec/zvec-grep');
  const installed = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
  assert.equal(installed.version, info.version, 'dist-tag moved during install; start a fresh run');
  assert.equal(installed.name, '@zvec/zvec-grep');
  const candidate = productCandidate(root, prefix, installed);
  assert.equal(candidate.runtime, arm === 'node' ? 'node-0.2.2' : 'rust-native');
  assert.equal(new URL(info.dist.tarball).origin, registry);
  const download = await fetch(info.dist.tarball);
  assert.ok(download.ok, `npm tarball unavailable: ${download.status}`);
  const bytes = Buffer.from(await download.arrayBuffer());
  assert.equal(`sha512-${createHash('sha512').update(bytes).digest('base64')}`, info.dist.integrity);
  if (arm === 'node') assert.equal(info.dist.integrity, NODE_RELEASE.integrity);
  const directory = join(output, 'packages', arm);
  await mkdir(directory, { recursive: true });
  const tarball = join(directory, 'candidate.tgz');
  await writeFile(tarball, bytes);
  metadata[arm] = {
    selector, version: installed.version, identity: `npm:@zvec/zvec-grep@${installed.version}`,
    runtime: candidate.runtime, registry_integrity: info.dist.integrity,
    tarball_url: info.dist.tarball, tarball_sha256: await fileHash(tarball),
    cli_sha256: await fileHash(candidate.cli), source_commit: info.gitHead ?? null,
  };
  await writeJson(join(output, 'identities.json'), metadata);
  const help = await runCandidate(candidate, ['--help', 'index']);
  return { ...candidate, tarball, flags: indexConcurrencyArguments(help.stdout) };
}

async function warmup(arm, candidate) {
  for (const model of models) {
    const dir = join(output, 'warmup', arm, model.split('/')[1]);
    const root = join(dir, 'corpus');
    const home = join(dir, 'runtime-home');
    await mkdir(root, { recursive: true });
    await writeFile(join(root, 'example.py'), 'def example(value):\n    return value + 1\n');
    await mkdir(join(home, '.zvec-grep'), { recursive: true });
    await writeJson(join(home, '.zvec-grep/config.json'), { version: 1, defaults: { embedding: model, modelCacheDir: modelCache }, models: { [model]: { device: 'cpu' } } });
    const env = { ...process.env, HOME: home, USERPROFILE: home, ZVEC_GREP_HOME: join(home, '.zvec-grep'), ZVEC_GREP_MODEL_CACHE: modelCache, ZVEC_GREP_DEVICE: 'cpu', NO_COLOR: '1' };
    console.log(`Prewarming ${arm}: ${model}`);
    const result = await runCandidate(candidate, ['--index', root, '--mode', 'direct', '--embedding', model, '--model-cache', modelCache, '--device', 'cpu', ...candidate.flags], { cwd: root, env, timeout: 1200000 });
    await writeJson(join(dir, 'index.json'), result);
  }
}

async function suite(arm, name, candidate, python) {
  await sampleHost();
  assert.ok(hostSamples.at(-1).runner_workers <= 1, 'another runner job is active on this host');
  const directory = join(output, arm, name);
  const args = name === 'zg'
    ? ['benchmarks/zg-retrieval/run.mjs', '--corpus', join(output, 'corpus', arm)]
    : ['benchmarks/zg-retrieval/expansion/run.mjs', '--suite', name];
  args.push('--package', candidate.tarball, '--output', directory, '--model-cache', modelCache, '--candidate-commit', metadata[arm].identity);
  const env = { ...process.env, RETRIEVAL_EMBEDDING: 'local', RETRIEVAL_INSTALLED_PACKAGE: candidate.packageRoot, PATH: `${python ? `${python}${delimiter}` : ''}${process.env.PATH}` };
  try {
    await execute(`${arm}-${name}`, process.execPath, args, env);
  } catch (error) { failures.push(error.message); }
  try { bundles[arm].reports[name] = JSON.parse(await readFile(join(directory, 'report.json'), 'utf8')); }
  catch (error) { failures.push(`${arm}/${name}: report unavailable (${error.code ?? error.message})`); }
}

let comparison;
try {
  const candidates = { node: await install('node', '0.2.2'), rust: await install('rust', 'rust-canary') };
  const pythons = {};
  for (const name of ['beir', 'duretrieval']) {
    const venv = join(output, 'venvs', name);
    await execute(`venv-${name}`, 'python3', ['-m', 'venv', venv]);
    await execute(`pip-${name}`, join(venv, 'bin/python'), ['-m', 'pip', 'install', '--requirement', `benchmarks/zg-retrieval/expansion/requirements-${name}.txt`]);
    pythons[name] = join(venv, 'bin');
  }
  await mkdir(modelCache, { recursive: true });
  for (const arm of ['node', 'rust']) await warmup(arm, candidates[arm]);
  // Pair by suite to reduce drift between the two products, with no overlap.
  for (const name of suites)
    for (const arm of ['node', 'rust']) await suite(arm, name, candidates[arm], pythons[name]);
  for (const arm of ['node', 'rust']) {
    bundles[arm].summary = await buildCombined({ ...bundles[arm].reports, candidateCommit: metadata[arm].identity, candidateRef: metadata[arm].identity, harnessCommit: metadata.harness_commit });
    await writeJson(join(output, arm, 'summary.json'), bundles[arm].summary);
  }
  assert.equal(failures.length, 0, failures.join('; '));
  for (const arm of ['node', 'rust']) validatePerformanceCoverage(bundles[arm].reports);
  assert.ok(hostSamples.every(s => s.runner_workers !== null && s.runner_workers <= 1), 'overlapping runner jobs detected; performance run is contaminated');
  comparison = await compareRuns(bundles.node, bundles.rust, {
    baseline: { repository: 'zvec-ai/zvec-grep', commit: metadata.node.identity },
    candidate: { repository: 'zvec-ai/zvec-grep', commit: metadata.rust.identity },
    run_url: process.env.GITHUB_RUN_ID ? `https://github.com/zvec-ai/zvec-grep/actions/runs/${process.env.GITHUB_RUN_ID}` : null,
  });
} catch (error) {
  failures.push(error.stack ?? error.message);
  comparison = { status: 'failed', errors: failures, rows: [] };
} finally {
  clearInterval(samplingTimer);
  metadata.finished_at = new Date().toISOString();
  await writeJson(join(output, 'identities.json'), metadata);
  await writeJson(join(output, 'host-samples.json'), hostSamples);
  await mkdir(join(output, 'comparison'), { recursive: true });
  await writeJson(join(output, 'comparison/comparison.json'), comparison);
  if (metadata.node && metadata.rust) {
    await writeFile(join(output, 'comparison/summary.md'), performanceReport(bundles, metadata, comparison));
    await writeFile(join(output, 'comparison/quality.md'), comparison.status === 'success' ? markdownVersionComparison(comparison) : '# Incomplete comparison\n\n' + failures.join('\n\n'));
  } else await writeFile(join(output, 'comparison/summary.md'), '# Retrieval-only: failed\n\n' + failures.join('\n\n'));
  await writeJson(join(output, 'comparison/summary.json'), { status: comparison.status, metadata, failures });
}
console.log(`Comparison: ${join(output, 'comparison/summary.md')}`);
if (comparison.status !== 'success') process.exitCode = 1;
