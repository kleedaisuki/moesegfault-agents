import { createParser } from "eventsource-parser";
import i18n, { locale } from "./i18n";
export type Agent = {
  id: string;
  name: string;
  model: {
    provider: string;
    model: string;
    context_window: number;
    max_output_tokens: number;
    credential_id?: string;
  };
  instructions: string;
  compaction_percent: number;
  workspace: boolean;
  skills: string[];
};
export type Model = {
  provider: string;
  model: string;
  context_window: number;
  max_output_tokens: number;
};
export type ModelRegistry = {
  data: { id: string }[];
  available_models: Model[];
  available_platform_models?: Model[];
};
export type ProviderKey = { id: string; provider: string };
export type Message = { role: string; content: string };
export type Session = {
  id: string;
  agent_id: string;
  status?: string;
  input_revision?: number;
  last_input_at_ms?: number;
  messages?: Message[];
  context_tokens?: number;
  context_window?: number;
  last_error?: string;
  pending_messages?: Message[];
};
export type Account = {
  account_id: string;
  name?: string;
  plan: string;
  billing_mode?: "test" | "live";
  credit_microusd?: number;
  csrf_token?: string;
  limits?: { agents?: number; concurrent_sessions?: number };
  subscription_url?: string;
};
export type Usage = {
  wallet: {
    budget_microusd: string;
    spent_microusd: string;
    reserved_microusd: string;
    available_microusd: string;
  };
  by_resource: { resource: string; cost_microusd: string }[];
};
export type ApiKey = {
  id: string;
  name: string;
  prefix: string;
  created_at?: string;
};
export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
    public requestId?: string,
    public code?: string,
  ) {
    super(message);
  }
}
let csrfToken: string | undefined;
export function setCsrfToken(token?: string) {
  csrfToken = token;
}
export async function responseFor(
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  const headers = new Headers(init.headers);
  if (init.body) headers.set("Content-Type", "application/json");
  if (csrfToken && init.method && !["GET", "HEAD"].includes(init.method))
    headers.set("X-CSRF-Token", csrfToken);
  const response = await fetch(path, {
    ...init,
    credentials: "same-origin",
    headers,
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new ApiError(
      body.error?.message ??
        body.detail ??
        body.message ??
        `Request failed (${response.status})`,
      response.status,
      response.headers.get("x-moesegfault-correlation-id") ??
        response.headers.get("x-request-id") ??
        undefined,
      body.error?.code ?? body.code,
    );
  }
  return response;
}
export async function request<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const response = await responseFor(path, init);
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}
export const json = (method: string, value?: unknown): RequestInit => ({
  method,
  ...(value === undefined ? {} : { body: JSON.stringify(value) }),
});
export function dollars(value?: number) {
  return value === undefined
    ? "—"
    : new Intl.NumberFormat(locale(), {
        style: "currency",
        currency: "USD",
        minimumFractionDigits: 2,
        maximumFractionDigits: 4,
      }).format(value);
}

export function apiBaseUrl(
  origin = window.location.origin,
  configuredOrigin = import.meta.env.VITE_AGENTS_API_ORIGIN,
): string {
  const url = new URL(configuredOrigin || origin);
  if (!configuredOrigin)
    url.hostname = url.hostname.replace(/^adash(?=[.-])/, "agents");
  return `${url.origin}/v1`;
}

export function localizedError(error: unknown): string {
  const message =
    error instanceof Error
      ? error.message
      : typeof error === "string"
        ? error
        : "";
  if (message && i18n.exists(message)) return i18n.t(message);
  if (i18n.resolvedLanguage === "en" && message) return message;
  if (error instanceof ApiError) {
    const key =
      error.status === 401
        ? "Sign in required"
        : error.status === 403
          ? "Permission denied"
          : error.status === 404
            ? "Not found"
            : error.status === 409
              ? "Conflict"
              : error.status === 429
                ? "Limit reached"
                : error.status >= 500
                  ? "Service unavailable"
                  : "Invalid request";
    return i18n.t(key);
  }
  return i18n.t(
    error instanceof TypeError
      ? "Network request failed"
      : "Something went wrong",
  );
}
export function formattedDate(value: number | string): string {
  const number = typeof value === "number" ? value : Number(value);
  const date = Number.isFinite(number)
    ? new Date(number < 1e12 ? number * 1000 : number)
    : new Date(value);
  return Number.isNaN(date.valueOf())
    ? "—"
    : new Intl.DateTimeFormat(locale(), { dateStyle: "medium" }).format(date);
}

export function microDollars(value?: string): string {
  if (value === undefined) return "—";
  if (!/^-?\d+$/.test(value)) return "—";
  const money = BigInt(value),
    absolute = money < 0n ? -money : money,
    whole = absolute / 1_000_000n;
  const fraction = (absolute % 1_000_000n)
    .toString()
    .padStart(6, "0")
    .replace(/0{1,4}$/, "");
  const integer = money < 0n ? (whole === 0n ? -0 : -whole) : whole;
  return new Intl.NumberFormat(locale(), {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
  })
    .formatToParts(integer)
    .map((part) => (part.type === "fraction" ? fraction : part.value))
    .join("");
}
export function usageTotals(usage?: Usage) {
  if (!usage) return undefined;
  const result = { total: 0n, model: 0n, container: 0n, storage: 0n };
  for (const row of usage.by_resource) {
    const value = BigInt(row.cost_microusd);
    result.total += value;
    if (row.resource === "model") result.model += value;
    if (row.resource === "container") result.container += value;
    if (row.resource === "r2_storage" || row.resource === "r2_operation")
      result.storage += value;
  }
  return result;
}

export type Skill = {
  source?: "account" | "deployment";
  name: string;
  version: string;
  instructions: string;
  updated_at?: number;
};

export async function streamCompletion(
  path: string,
  init: RequestInit,
  onDelta: (text: string) => void,
): Promise<void> {
  const response = await responseFor(path, init);
  if (
    !response.headers.get("content-type")?.includes("text/event-stream") ||
    !response.body
  )
    throw new Error("Invalid response stream");
  let completed = false;
  const parser = createParser({
    onEvent: (event) => {
      if (event.data === "[DONE]") {
        completed = true;
        return;
      }
      const value = JSON.parse(event.data);
      if (value.error)
        throw new ApiError(
          value.error.message ?? "Request failed",
          502,
          undefined,
          value.error.code,
        );
      const content = value.choices?.[0]?.delta?.content;
      if (typeof content === "string") onDelta(content);
    },
  });
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      parser.feed(decoder.decode(value, { stream: true }));
    }
    parser.feed(decoder.decode());
    if (!completed) throw new Error("Response stream interrupted");
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}
