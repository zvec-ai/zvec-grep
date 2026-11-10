// Run after the root Node build, in a private home with disposable files.
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "../../dist/cli/args.js";
import { createServiceOptions } from "../../dist/cli/commands.js";
import { resolveClientToken, resolveServerToken } from "../../dist/daemon/config.js";

const fixturePath = fileURLToPath(
  new URL("../compat/cli/server-option-precedence.json", import.meta.url),
);
const fixture = JSON.parse(await readFile(fixturePath, "utf8"));
assert.equal(fixture.schema_version, 1);
const previousDirectory = process.cwd();
const directory = await mkdtemp(join(tmpdir(), "server-option-oracle-"));
const variables = [
  "ZVEC_GREP_HOME", "ZVEC_GREP_SERVER_TOKEN_FILE", "ZVEC_GREP_SERVER_TOKEN",
];
const original = new Map(variables.map((name) => [name, process.env[name]]));
const tokens = new Map();
try {
  process.chdir(directory);
  for (const name of ["before-action", "after-action", "environment-token"]) {
    const token = `fixture-only-${name}-00000000000000000000000000000000`;
    tokens.set(token, name);
    await writeFile(name, `${token}\n`, { mode: 0o600 });
  }
  for (const entry of fixture.cases) {
    for (const name of variables) delete process.env[name];
    for (const [name, value] of Object.entries(entry.env)) {
      assert(variables.slice(0, 2).includes(name), `unexpected environment key: ${name}`);
      process.env[name] = value;
    }
    let parsed;
    try {
      parsed = parseArgs(entry.argv.slice(1));
    } catch (error) {
      entry.node_result = {
        kind: "parse_error",
        message: error instanceof Error ? error.message : String(error),
      };
      console.log(`SERVER_OPTION_ORACLE ${JSON.stringify({ case: entry.id, result: entry.node_result })}`);
      continue;
    }
    assert.equal(parsed.command, "server");
    assert.equal(parsed.options.serverAction, entry.action);
    let selected;
    if (entry.option === "--home") {
      selected = createServiceOptions(parsed.options, undefined).home;
    } else {
      assert.equal(entry.option, "--token-file");
      const options = { tokenFile: parsed.options.serverTokenFile };
      const server = await resolveServerToken(options);
      selected = server.tokenFile;
      if (entry.action === "off") {
        const client = await resolveClientToken(options);
        assert.equal(client === undefined ? undefined : tokens.get(client), selected);
      }
    }
    entry.node_result = selected === undefined
      ? { kind: "unset" }
      : { kind: "selected", value: selected };
    console.log(`SERVER_OPTION_ORACLE ${JSON.stringify({ case: entry.id, result: entry.node_result })}`);
  }
  await writeFile(fixturePath, `${JSON.stringify(fixture, null, 2)}\n`);
} finally {
  process.chdir(previousDirectory);
  for (const [name, value] of original) {
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
  await rm(directory, { recursive: true, force: true });
}
