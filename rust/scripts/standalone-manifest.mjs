const metadataKeys = ["schema", "version", "tag", "commit", "partial"];

// Release metadata has two columns; archive records have five. No escaping or evaluation is used.
export function parseStandaloneManifest(text) {
  const lines = text.replaceAll("\r\n", "\n").split("\n");
  if (lines.at(-1) === "") lines.pop();
  const metadata = new Map();
  const assets = [];
  const identities = new Set();
  const fail = (reason) => { throw new Error(`invalid release manifest: ${reason}`); };
  for (const line of lines) {
    const fields = line.split("\t");
    if (metadataKeys.includes(fields[0])) {
      if (fields.length !== 2 || metadata.has(fields[0])) fail(`invalid or duplicate release ${fields[0]}`);
      metadata.set(fields[0], fields[1]);
    } else {
      if (fields.length !== 5) fail("invalid archive columns");
      const [target, accelerator, file, sha256, size] = fields;
      const identity = `${target}/${accelerator}`;
      if (!/^[a-z0-9]+(?:-[a-z0-9]+)+$/.test(target) || !["cpu", "cuda"].includes(accelerator) ||
          identities.has(identity) || !/^[a-f0-9]{64}$/.test(sha256) ||
          !/^[1-9][0-9]*$/.test(size) || !Number.isSafeInteger(Number(size))) fail("invalid or duplicate archive metadata");
      identities.add(identity);
      assets.push({ target, accelerator, file, sha256, bytes: Number(size) });
    }
  }
  if (metadata.get("schema") !== "1") fail("unsupported schema");
  const version = metadata.get("version");
  const tag = metadata.get("tag");
  const commit = metadata.get("commit");
  const partial = metadata.get("partial");
  if (!version || !/^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?$/.test(version)) fail("invalid release version");
  if (!tag || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(tag)) fail("invalid release tag");
  if (!commit || !/^[a-f0-9]{40}$/.test(commit)) fail("invalid source commit");
  if (!["true", "false"].includes(partial)) fail("invalid partial flag");
  if (!assets.length) fail("missing archives");
  for (const asset of assets) {
    const extension = asset.target.startsWith("win32-") ? "zip" : "tar.gz";
    if (asset.file !== `zvec-grep-${version}-${asset.target}-${asset.accelerator}.${extension}`) fail("invalid archive filename");
  }
  return { schemaVersion: 1, version, tag, commit, partial: partial === "true", assets };
}

export function serializeStandaloneManifest(release) {
  const table = `schema\t${release.schemaVersion}\nversion\t${release.version}\ntag\t${release.tag}\ncommit\t${release.commit}\npartial\t${release.partial}\n` +
    release.assets.map((asset) => `${asset.target}\t${asset.accelerator}\t${asset.file}\t${asset.sha256}\t${asset.bytes}\n`).join("");
  parseStandaloneManifest(table);
  return table;
}
