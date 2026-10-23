import { ApiError, type Session } from "./api";
export function sessionPollInterval(
  data: Session | undefined,
  error: unknown,
  failureCount: number,
  now = Date.now(),
): number | false {
  if (data?.status === "closed") return false;
  if (error instanceof ApiError && [401, 403, 404].includes(error.status))
    return false;
  if (error) return Math.min(30_000, 2_000 * 2 ** Math.min(failureCount, 4));
  if (data?.status === "running" || data?.pending_messages?.length)
    return 2_000;
  const idleFor = Math.max(0, now - (data?.last_input_at_ms ?? now));
  return idleFor < 15_000 ? 5_000 : idleFor < 60_000 ? 15_000 : 30_000;
}
