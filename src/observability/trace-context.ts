import { AsyncLocalStorage } from "node:async_hooks";

export type TraceContext = {
  traceparent: string;
  traceId: string;
  tracestate?: string;
  baggage?: string;
};

const storage = new AsyncLocalStorage<TraceContext>();
const TRACEPARENT =
  /^([0-9a-f]{2})-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})((?:-[0-9a-f]+)*)$/;

export function currentTraceContext(): TraceContext | undefined {
  return storage.getStore();
}

export function runWithTraceContext<T>(
  context: TraceContext | undefined,
  operation: () => T,
): T {
  return context ? storage.run(context, operation) : operation();
}

export function traceContextFromMcpBody(
  body: unknown,
): TraceContext | undefined {
  if (!body || typeof body !== "object" || Array.isArray(body)) {
    return undefined;
  }
  return traceContextFromMcpMeta(
    (body as { params?: { _meta?: unknown } }).params?._meta,
  );
}

export function traceContextFromMcpMeta(
  meta: unknown,
): TraceContext | undefined {
  if (!meta || typeof meta !== "object" || Array.isArray(meta)) {
    return undefined;
  }
  const values = meta as Record<string, unknown>;
  const traceparent =
    typeof values.traceparent === "string" ? values.traceparent : undefined;
  const match = traceparent?.match(TRACEPARENT);
  if (
    !match ||
    match[1] === "ff" ||
    (match[1] === "00" && match[5] !== "") ||
    match[2] === "0".repeat(32) ||
    match[3] === "0".repeat(16)
  ) {
    return undefined;
  }
  const tracestate = validTracestate(values.tracestate);
  const baggage = validBaggage(values.baggage);
  return {
    traceparent: traceparent!,
    traceId: match[2]!,
    ...(tracestate ? { tracestate } : {}),
    ...(baggage ? { baggage } : {}),
  };
}

export function traceHeaders(): Record<string, string> {
  const context = currentTraceContext();
  if (!context) return {};
  return {
    traceparent: context.traceparent,
    ...(context.tracestate ? { tracestate: context.tracestate } : {}),
    ...(context.baggage ? { baggage: context.baggage } : {}),
  };
}

function boundedHeader(
  value: unknown,
  maxLength: number,
  allowHorizontalTab = false,
): string | undefined {
  if (typeof value !== "string" || value.length > maxLength) return undefined;
  for (const character of value) {
    const code = character.charCodeAt(0);
    if (
      code > 0x7e ||
      (code < 0x20 && !(allowHorizontalTab && code === 0x09))
    ) {
      return undefined;
    }
  }
  return value;
}

function validTracestate(value: unknown): string | undefined {
  const header = boundedHeader(value, 512);
  if (!header) return undefined;
  const members = header.split(",");
  if (members.length > 32) return undefined;
  const keys = new Set<string>();
  for (const rawMember of members) {
    const member = rawMember.trim();
    const separator = member.indexOf("=");
    if (separator <= 0) return undefined;
    const key = member.slice(0, separator);
    const item = member.slice(separator + 1);
    if (
      !/^(?:[a-z][a-z0-9_\-*/]{0,255}|[a-z0-9][a-z0-9_\-*/]{0,240}@[a-z][a-z0-9_\-*/]{0,13})$/.test(
        key,
      ) ||
      item.length === 0 ||
      item.length > 256 ||
      !/^[\x20-\x2b\x2d-\x3c\x3e-\x7e]+$/.test(item) ||
      item.endsWith(" ") ||
      keys.has(key)
    ) {
      return undefined;
    }
    keys.add(key);
  }
  return header;
}

function validBaggage(value: unknown): string | undefined {
  const header = boundedHeader(value, 8_192, true);
  if (!header) return undefined;
  const members = header.split(",");
  if (members.length > 64) return undefined;
  for (const rawMember of members) {
    const segments = rawMember.trim().split(";");
    const pair = segments.shift()!;
    const separator = pair.indexOf("=");
    const pairValue = separator < 0 ? "" : pair.slice(separator + 1).trim();
    if (
      separator <= 0 ||
      !isHttpToken(pair.slice(0, separator).trim()) ||
      hasInvalidBaggageCharacters(pairValue, ",")
    ) {
      return undefined;
    }
    for (const rawProperty of segments) {
      const property = rawProperty.trim();
      const propertySeparator = property.indexOf("=");
      const key =
        propertySeparator < 0
          ? property
          : property.slice(0, propertySeparator).trim();
      const propertyValue =
        propertySeparator < 0
          ? ""
          : property.slice(propertySeparator + 1).trim();
      if (
        !isHttpToken(key) ||
        hasInvalidBaggageCharacters(propertyValue, ";,")
      ) {
        return undefined;
      }
    }
  }
  return header;
}

function isHttpToken(value: string): boolean {
  return /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(value);
}

function hasInvalidBaggageCharacters(
  value: string,
  delimiters: string,
): boolean {
  for (const character of value) {
    const code = character.charCodeAt(0);
    if (
      code <= 0x20 ||
      code === 0x22 ||
      code === 0x5c ||
      code === 0x7f ||
      delimiters.includes(character)
    ) {
      return true;
    }
  }
  return false;
}
