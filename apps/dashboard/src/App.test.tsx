import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { ApiError, apiBaseUrl, request, setCsrfToken } from "./api";

const account = {
  account_id: "acct_1",
  plan: "free",
  csrf_token: "csrf_example",
  limits: { agents: 3, concurrent_sessions: 1 },
};
const agent = {
  id: "agent_1",
  name: "Research companion",
  instructions: "Be careful",
  model: {
    provider: "openrouter",
    model: "example/model",
    context_window: 200000,
    max_output_tokens: 8192,
  },
  compaction_percent: 80,
  workspace: true,
  skills: [],
};
const calls: { path: string; init: RequestInit }[] = [];
function mockApi(routes: Record<string, unknown>) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: string, init: RequestInit = {}) => {
      calls.push({ path: input, init });
      if (input === "/v1/skills" && !init.method)
        return new Response(JSON.stringify({ data: [] }));
      const key = `${init.method ?? "GET"} ${input}`;
      if (!(key in routes))
        return new Response(
          JSON.stringify({ error: { message: `No test route: ${key}` } }),
          { status: 404 },
        );
      const value = routes[key];
      return value instanceof Response
        ? value.clone()
        : new Response(JSON.stringify(value), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
    }),
  );
}
function mount() {
  return render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: {
            queries: { retry: false },
            mutations: { retry: false },
          },
        })
      }
    >
      <App />
    </QueryClientProvider>,
  );
}
beforeEach(() => {
  calls.length = 0;
  window.history.replaceState(null, "", "/");
  setCsrfToken(undefined);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});
describe("workspace journeys", () => {
  it("sends unauthenticated users to the BFF without browser OAuth credentials", async () => {
    mockApi({
      "GET /v1/account": new Response('{"error":{"message":"Sign in"}}', {
        status: 401,
      }),
    });
    mount();
    expect(
      await screen.findByRole("link", { name: /Continue with moeSegFault/ }),
    ).toHaveAttribute("href", "/auth/login?return_to=/");
    expect(calls[0].init.credentials).toBe("same-origin");
    expect(calls[0].init.headers).not.toHaveProperty("Authorization");
  });
  it("creates an agent with percentage compaction and pinned instructions", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [] },
      "GET /v1/models": {
        data: [],
        available_models: [
          {
            provider: "openrouter",
            model: "example/model",
            context_window: 200000,
            max_output_tokens: 8192,
          },
        ],
      },
      "POST /v1/agents": agent,
    });
    mount();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "New agent" }));
    const dialog = screen.getByRole("dialog");
    await user.type(
      within(dialog).getByLabelText("Name"),
      "Research companion",
    );
    await user.type(within(dialog).getByLabelText("Model"), "example/model");
    await user.type(
      within(dialog).getByLabelText("Instructions"),
      "Be careful",
    );
    await user.click(
      within(dialog).getByRole("button", { name: "Save agent" }),
    );
    await waitFor(() =>
      expect(
        calls.some((c) => c.path === "/v1/agents" && c.init.method === "POST"),
      ).toBe(true),
    );
    const call = calls.find(
      (c) => c.path === "/v1/agents" && c.init.method === "POST",
    )!;
    expect(JSON.parse(call.init.body as string)).toMatchObject({
      compaction_percent: 80,
      instructions: "Be careful",
      workspace: true,
      tools: [
        "time",
        "bash",
        "workspace_commit",
        "workspace_versions",
        "workspace_restore",
      ],
      model: { model: "example/model", context_window: 200000 },
    });
    expect(new Headers(call.init.headers).get("X-CSRF-Token")).toBe(
      "csrf_example",
    );
  });
  it("surfaces backend failures instead of pretending an agent was saved", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [] },
      "GET /v1/models": {
        data: [],
        available_models: [
          {
            provider: "openrouter",
            model: "model",
            context_window: 200000,
            max_output_tokens: 8192,
          },
        ],
      },
      "POST /v1/agents": new Response(
        '{"error":{"message":"Agent limit reached"}}',
        { status: 429 },
      ),
    });
    mount();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "New agent" }));
    await user.type(screen.getByLabelText("Name"), "New agent");
    await user.type(screen.getByLabelText("Model"), "model");
    await user.click(screen.getByRole("button", { name: "Save agent" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Agent limit reached",
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });
  it("starts a session and queues steering while preserving the session ID", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [agent] },
      "GET /v1/agents/agent_1/sessions": { data: [] },
      "POST /v1/agents/agent_1/sessions": { id: "session_1" },
      "GET /v1/sessions/session_1/view": {
        id: "session_1",
        agent_id: "agent_1",
        status: "running",
        message_count: 1,
      },
      "GET /v1/sessions/session_1/messages?limit=50": {
        object: "list",
        data: [
          {
            sequence: 0,
            message: { role: "assistant", content: "Working on it" },
          },
        ],
        start: 0,
        end: 1,
        high_water: 1,
        before_cursor: "before-0",
        after_cursor: "after-1",
        has_older: false,
        has_newer: false,
      },
      "POST /v1/sessions/session_1/messages": { queued: true },
    });
    mount();
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Research companion" }),
    );
    await user.click(screen.getByRole("button", { name: "Start a session" }));
    expect(await screen.findByText("Working on it")).toBeInTheDocument();
    await user.type(
      screen.getByRole("textbox", { name: "Message or steering instruction" }),
      "Focus on the tests",
    );
    await user.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(
        calls.find((c) => c.path === "/v1/sessions/session_1/messages"),
      ).toBeDefined(),
    );
    expect(
      JSON.parse(
        calls.find((c) => c.path.endsWith("/messages"))!.init.body as string,
      ),
    ).toEqual({ content: "Focus on the tests" });
  });
  it("requires explicit diagnostic consent and sends only the reviewed payload", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [] },
      "GET /v1/api-keys": { data: [] },
      "GET /v1/provider-keys": { data: [] },
      "POST /v1/diagnostics": { ok: true },
    });
    mount();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Access" }));
    const send = await screen.findByRole("button", {
      name: "Send diagnostics",
    });
    expect(send).toBeDisabled();
    await user.type(screen.getByLabelText("Error code"), "request_failed");
    await user.click(screen.getByRole("checkbox", { name: /I authorize/ }));
    await user.click(send);
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Diagnostic received",
    );
    const body = JSON.parse(
      calls.find((c) => c.path === "/v1/diagnostics")!.init.body as string,
    );
    expect(body).toEqual({
      consent: true,
      client: "adash",
      version: "0.3.0",
      platform: "browser",
      error_code: "request_failed",
    });
    expect(JSON.stringify(body)).not.toContain("csrf_example");
    expect(JSON.stringify(body)).not.toContain("userAgent");
  });
});
describe("session recovery and credentials", () => {
  it("resumes an archived session before allowing new messages", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [agent] },
      "GET /v1/agents/agent_1/sessions": {
        data: [{ id: "old-session", status: "closed" }],
      },
      "GET /v1/sessions/old-session/view": {
        id: "old-session",
        agent_id: agent.id,
        status: "closed",
        message_count: 0,
      },
      "GET /v1/sessions/old-session/messages?limit=50": {
        object: "list",
        data: [],
        start: 0,
        end: 0,
        high_water: 0,
        before_cursor: "before-0",
        after_cursor: "after-0",
        has_older: false,
        has_newer: false,
      },
      "POST /v1/sessions/old-session/resume": {
        id: "old-session",
        status: "idle",
      },
    });
    mount();
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Research companion" }),
    );
    await user.click(await screen.findByRole("button", { name: /old-sess/ }));
    const resume = await screen.findByRole("button", {
      name: "Resume session",
    });
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    await user.click(resume);
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.path === "/v1/sessions/old-session/resume" &&
            c.init.method === "POST",
        ),
      ).toBe(true),
    );
  });
  it("shows a newly generated key once and forgets it on acknowledgement", async () => {
    mockApi({
      "GET /v1/account": account,
      "GET /v1/agents": { data: [] },
      "GET /v1/api-keys": { data: [] },
      "GET /v1/provider-keys": { data: [] },
      "POST /v1/api-keys": {
        id: "key-1",
        name: "Notebook",
        key: "mag_test_only_secret",
      },
    });
    mount();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Access" }));
    await user.type(screen.getByLabelText("New API key name"), "Notebook");
    await user.click(screen.getByRole("button", { name: "Create key" }));
    expect(await screen.findByLabelText("New API key")).toHaveValue(
      "mag_test_only_secret",
    );
    await user.click(screen.getByRole("button", { name: "I’ve saved it" }));
    expect(screen.queryByLabelText("New API key")).not.toBeInTheDocument();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
  });
});

describe("API transport", () => {
  it("preserves a support request ID on errors", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response('{"detail":"Capacity exhausted"}', {
            status: 503,
            headers: { "x-moesegfault-correlation-id": "req_1" },
          }),
      ),
    );
    await expect(request("/v1/agents")).rejects.toMatchObject({
      status: 503,
      requestId: "req_1",
      message: "Capacity exhausted",
    } satisfies Partial<ApiError>);
  });
  it("handles successful empty responses", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(null, { status: 204 })),
    );
    await expect(
      request("/v1/agents/1", { method: "DELETE" }),
    ).resolves.toBeUndefined();
  });
});

describe("environment-aware API snippet", () => {
  it("uses the matching public API host without crossing environments", () => {
    expect(apiBaseUrl("https://adash-staging.moesegfault.dev", "")).toBe(
      "https://agents-staging.moesegfault.dev/v1",
    );
    expect(apiBaseUrl("https://adash.moesegfault.dev", "")).toBe(
      "https://agents.moesegfault.dev/v1",
    );
    expect(apiBaseUrl("http://127.0.0.1:5173", "")).toBe(
      "http://127.0.0.1:5173/v1",
    );
    expect(
      apiBaseUrl("https://preview.example", "https://api-staging.example"),
    ).toBe("https://api-staging.example/v1");
  });
});
