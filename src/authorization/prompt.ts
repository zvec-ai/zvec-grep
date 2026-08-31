import { basename } from "node:path";
import type { RemoteEmbeddingDataDisclosure } from "./types.js";

export type RemoteEmbeddingAuthorizationPromptInput = {
  workspaceRoots: readonly string[];
  provider: string;
  model: string;
  endpoint?: string;
  data: readonly string[];
  note?: string;
};

export const REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE =
  "The connected MCP host does not support the Remote Embedding authorization interaction required by elicitation/create. The Agent should use AskUserQuestion to ask the user to choose: allow Remote Embedding for this workspace, use local FTS only, or cancel. No user decision was received, and no remote data was sent.";

export function remoteEmbeddingDisclosureData(
  disclosure: RemoteEmbeddingDataDisclosure,
): string[] {
  return [
    ...(disclosure.queryText ? ["query text"] : []),
    ...(disclosure.workspaceContent === "selected"
      ? ["selected workspace files"]
      : disclosure.workspaceContent === "changed"
        ? ["changed workspace files"]
        : disclosure.workspaceContent === "full"
          ? ["selected workspace files"]
          : []),
  ];
}

export function formatRemoteEmbeddingAuthorizationPrompt(
  input: RemoteEmbeddingAuthorizationPromptInput,
): string {
  const data = naturalList(
    input.data.length > 0 ? input.data : ["data required by this operation"],
  );
  const lines = [
    "Remote Embedding authorization",
    "",
    `Send ${data}?`,
    "",
    `  From  ${workspaceLabel(input.workspaceRoots)}`,
    `  To    ${clip(`${input.provider}/${input.model}`, 72)}`,
  ];
  const host = endpointHost(input.endpoint);
  if (host) lines.push(`        ${clip(host, 72)}`);
  if (input.note) lines.push("", input.note);
  lines.push("", "API charges may apply.");
  return lines.join("\n");
}

function workspaceLabel(roots: readonly string[]): string {
  const names = roots.map((root) =>
    clip(basename(root) || root || "workspace", 32),
  );
  if (names.length === 0) return "workspace";
  if (names.length <= 2) return names.join(", ");
  return `${names.slice(0, 2).join(", ")} +${names.length - 2}`;
}

function endpointHost(endpoint: string | undefined): string | undefined {
  if (!endpoint) return undefined;
  try {
    return new URL(endpoint).host;
  } catch {
    return endpoint.replace(/^https?:\/\//, "").split("/")[0] || undefined;
  }
}

function naturalList(items: readonly string[]): string {
  if (items.length <= 1) return items[0] ?? "data";
  if (items.length === 2) return `${items[0]} and ${items[1]}`;
  return `${items.slice(0, -1).join(", ")}, and ${items.at(-1)}`;
}

function clip(value: string, maxLength: number): string {
  if (value.length <= maxLength) return value;
  return `${value.slice(0, Math.max(1, maxLength - 1))}…`;
}
