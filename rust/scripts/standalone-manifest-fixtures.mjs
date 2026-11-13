// The same malformed records exercise the build parser and both dependency-free installers.
export function invalidManifestTables(table) {
  const archive = table.trimEnd().split("\n").at(-1);
  const fields = archive.split("\t");
  const replaceArchive = (index, value) => table.replace(archive, fields.map((field, i) => i === index ? value : field).join("\t"));
  return [
    ...["schema", "version", "tag", "commit", "partial"].flatMap((key) => {
      const row = table.split("\n").find((line) => line.startsWith(`${key}\t`));
      return [[`missing ${key}`, table.replace(`${row}\n`, "")], [`duplicate ${key}`, `${table}${row}\n`]];
    }),
    ["unsupported schema", table.replace("schema\t1", "schema\t2")],
    ["noncanonical schema", table.replace("schema\t1", "schema\t01")],
    ["invalid version", table.replace(/version\t[^\n]+/, "version\t../escape")],
    ["invalid commit", table.replace(/commit\t[^\n]+/, "commit\tfixture")],
    ["non-boolean partial", table.replace(/partial\t[^\n]+/, "partial\t0")],
    ["unknown metadata", `${table}unknown\tvalue\n`],
    ["metadata extra column", table.replace("schema\t1", "schema\t1\textra")],
    ["blank record", `${table}\n`],
    ["missing archives", table.split("\n").filter((line) => line.split("\t").length !== 5).join("\n")],
    ["duplicate archive", `${table}${archive}\n`],
    ["missing archive column", table.replace(archive, fields.slice(0, 4).join("\t"))],
    ["extra archive column", table.replace(archive, `${archive}\textra`)],
    ["invalid target", replaceArchive(0, "../escape")],
    ["invalid accelerator", replaceArchive(1, "CPU")],
    ["unsafe filename", replaceArchive(2, `../${fields[2]}`)],
    ["invalid digest", replaceArchive(3, "g".repeat(64))],
    ["uppercase digest", replaceArchive(3, "A".repeat(64))],
    ...["0", "-1", "1.5", "1e3", "01", "9007199254740992"].map((size) => [`invalid size ${size}`, replaceArchive(4, size)]),
  ];
}
