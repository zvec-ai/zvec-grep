const originalFetch = globalThis.fetch;

globalThis.fetch = async (input, init) => {
  const url =
    typeof Request !== "undefined" && input instanceof Request
      ? input.url
      : String(input);
  if (
    !url.startsWith("https://huggingface.co/") &&
    !url.startsWith("https://modelscope.cn/models/")
  ) {
    return await originalFetch(input, init);
  }

  if (process.env.ZVEC_GREP_TEST_MODEL_DOWNLOAD_FAILURE === "1") {
    return new Response("Injected model download failure", {
      status: 503,
      statusText: "Service Unavailable",
    });
  }

  const size = url.endsWith("/tokenizer.json") ? 128 : 1024;
  const bytes = new Uint8Array(size);
  bytes.fill(1);
  return new Response(bytes, {
    status: 200,
    headers: {
      "content-length": String(bytes.byteLength),
      "content-type": "application/octet-stream",
    },
  });
};
