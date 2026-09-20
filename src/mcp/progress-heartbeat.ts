import type { ProgressNotification } from "@modelcontextprotocol/server";

export const REMOTE_AUTHORIZATION_HEARTBEAT_MS = 15_000;

// The MCP SDK does not currently expose a disabled-timeout mode. Keep a
// practical upper bound as a safety net while progress heartbeats make the
// authorization wait open-ended.
export const LONG_RUNNING_MCP_TIMEOUT_MS = 2_147_483_647;

type ProgressHeartbeatContext = {
  mcpReq: {
    _meta?: { progressToken?: string | number };
    signal: AbortSignal;
    notify: (notification: ProgressNotification) => Promise<void>;
  };
};

export async function withProgressHeartbeat<T>(
  context: ProgressHeartbeatContext,
  operation: () => Promise<T>,
  options: {
    intervalMs?: number;
    message: string;
  },
): Promise<T> {
  const progressToken = context.mcpReq._meta?.progressToken;
  if (progressToken === undefined) {
    return await operation();
  }

  const intervalMs = options.intervalMs ?? REMOTE_AUTHORIZATION_HEARTBEAT_MS;
  let progress = Date.now();
  let inFlight: Promise<void> | undefined;

  const sendHeartbeat = async (): Promise<void> => {
    if (context.mcpReq.signal.aborted || inFlight) return;
    progress += 1;
    inFlight = context.mcpReq
      .notify({
        method: "notifications/progress",
        params: {
          progressToken,
          progress,
          message: options.message,
        },
      })
      .catch(() => undefined)
      .finally(() => {
        inFlight = undefined;
      });
    await inFlight;
  };

  await sendHeartbeat();
  const timer = setInterval(() => {
    void sendHeartbeat();
  }, intervalMs);
  timer.unref();

  try {
    return await operation();
  } finally {
    clearInterval(timer);
    await inFlight?.catch(() => undefined);
  }
}
