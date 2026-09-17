import { existsSync, writeFileSync } from "node:fs";
import { setTimeout } from "node:timers/promises";

const originalFetch = globalThis.fetch;
const gate = process.env.ZVEC_TEST_DOWNLOAD_GATE;
if (!gate) throw new Error("A test download gate is required");

globalThis.fetch = async (input, init) => {
  const url =
    typeof Request !== "undefined" && input instanceof Request
      ? input.url
      : String(input);
  if (
    !url.startsWith("https://huggingface.co/") &&
    !url.startsWith("https://modelscope.cn/")
  )
    return await originalFetch(input, init);

  writeFileSync(`${gate}.started`, "started");
  const deadline = Date.now() + 30_000;
  while (!existsSync(gate) && Date.now() < deadline) await setTimeout(100);
  throw new Error("simulated slow model download failure");
};
