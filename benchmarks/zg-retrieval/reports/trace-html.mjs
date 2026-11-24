import { readFile } from "node:fs/promises";

/** Render a self-contained trace report; evidence remains inert JSON. */
export async function renderGoldTrace(data) {
  const template = await readFile(
    new URL("trace-template.html", import.meta.url),
    "utf8",
  );
  const json = JSON.stringify(data).replace(
    /[<>&\u2028\u2029]/g,
    (character) =>
      `\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`,
  );
  return template.replace("__TRACE_DATA__", () => json);
}
