import { Profiler, type ProfilerOnRenderCallback } from "react";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, expect, it, vi } from "vitest";
import { Conversation } from "./Conversation";
import { setCsrfToken, type Agent, type MessageContent } from "./api";
import i18n from "./i18n";

const renders = vi.hoisted(() => ({ history: 0 }));
vi.mock("./MessageContent", async (original) => {
  const actual = await original<typeof import("./MessageContent")>();
  return {
    MessageContent: (props: { content: MessageContent }) => {
      if (
        typeof props.content === "string" &&
        props.content.startsWith("history-")
      )
        renders.history++;
      return <actual.MessageContent {...props} />;
    },
  };
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  setCsrfToken(undefined);
});

it("does not rerender stable source history for streamed text updates", async () => {
  await i18n.changeLanguage("en");
  setCsrfToken("test-csrf");
  renders.history = 0;
  const count = 100;
  const updates = 24;
  const entries = Array.from({ length: count }, (_, sequence) => ({
    sequence,
    message: {
      role: "tool",
      content: `history-${sequence}: ${"x".repeat(4096)}`,
    },
  }));
  const agent: Agent = {
    id: "agent-render",
    name: "Render workload",
    workspace: false,
    instructions: "Pinned instructions",
    compaction_percent: 73,
    skills: [],
    model: {
      provider: "deepseek",
      model: "flash",
      context_window: 100000,
      max_output_tokens: 8192,
    },
  };
  const session = {
    id: "session-render",
    agent_id: agent.id,
    status: "idle",
    message_count: count,
    latest_response_id: "resp-parent",
  };
  const page = (completed: boolean) => ({
    object: "list",
    data: completed
      ? [
          ...entries,
          { sequence: count, message: { role: "user", content: "Continue" } },
          {
            sequence: count + 1,
            message: { role: "assistant", content: "x".repeat(updates) },
          },
        ]
      : entries,
    start: 0,
    end: count + (completed ? 2 : 0),
    high_water: count + (completed ? 2 : 0),
    before_cursor: "before-0",
    after_cursor: `after-${count + (completed ? 2 : 0)}`,
    has_older: false,
    has_newer: false,
  });
  const cache = new QueryClient({
    defaultOptions: {
      queries: { retry: false, staleTime: 60000 },
      mutations: { retry: false },
    },
  });
  cache.setQueryData(["sessions", agent.id], { data: [session] });
  cache.setQueryData(["session", session.id], { session });
  cache.setQueryData(["session-messages", session.id], page(false));
  let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  let completed = false;
  const encoder = new TextEncoder();
  const commits: number[] = [];
  const profile: ProfilerOnRenderCallback = (_id, _phase, duration) => {
    commits.push(duration);
  };
  const calls: { path: string; method: string }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      calls.push({ path, method: init.method ?? "GET" });
      if (path === "/v1/responses")
        return new Response(
          new ReadableStream<Uint8Array>({
            start(value) {
              controller = value;
            },
          }),
          { headers: { "Content-Type": "text/event-stream" } },
        );
      if (path === `/v1/sessions/${session.id}/view`)
        return Response.json({
          ...session,
          message_count: count + (completed ? 2 : 0),
        });
      if (path.startsWith(`/v1/sessions/${session.id}/messages?`))
        return Response.json(page(completed));
      throw new Error(`Unexpected render-workload request: ${path}`);
    }),
  );
  render(
    <Profiler id="conversation" onRender={profile}>
      <QueryClientProvider client={cache}>
        <Conversation agent={agent} back={() => {}} />
      </QueryClientProvider>
    </Profiler>,
  );
  const user = userEvent.setup();
  const selectionStart = commits.length;
  await user.click(await screen.findByRole("button", { name: /session-/ }));
  await waitFor(() => expect(renders.history).toBeGreaterThanOrEqual(count));
  const initialHistoryDuration = commits
    .slice(selectionStart)
    .reduce((sum, value) => sum + value, 0);
  await user.type(
    screen.getByLabelText("Message or steering instruction"),
    "Continue",
  );
  await user.click(screen.getByRole("button", { name: "Send" }));
  await waitFor(() => expect(controller).toBeDefined());
  const before = renders.history;
  const beforeCommits = commits.length;
  for (let index = 0; index < updates; index++) {
    await act(async () => {
      controller!.enqueue(
        encoder.encode(
          `data: ${JSON.stringify({ type: "response.output_text.delta", delta: "x" })}\n\n`,
        ),
      );
    });
  }
  expect(screen.getByText("x".repeat(updates))).toBeInTheDocument();
  const repeatedHistoryRenders = renders.history - before;
  const updateCommits = commits.slice(beforeCommits);
  const report = {
    workload:
      "actual Conversation component, cached 100-entry source, 24 distinct async text updates",
    environment:
      "Vitest/jsdom React profiling; not browser paint or a network latency SLO",
    rows: count,
    tool_text_bytes_per_row: 4096,
    stream_updates: updates,
    repeated_history_renders: repeatedHistoryRenders,
    commits: updateCommits.length,
    react_actual_duration_ms: updateCommits.reduce(
      (sum, value) => sum + value,
      0,
    ),
    initial_history_actual_duration_ms: initialHistoryDuration,
    create_requests: calls.filter((call) => call.method === "POST").length,
  };
  console.log(
    JSON.stringify({ event: "dashboard.conversation_render", ...report }),
  );
  completed = true;
  await act(async () => {
    controller!.enqueue(
      encoder.encode(
        `data: ${JSON.stringify({ type: "response.completed", response: { id: "resp-new", status: "completed", output: [] } })}\n\n`,
      ),
    );
    controller!.close();
  });
  await waitFor(() =>
    expect(screen.queryByText("Working…")).not.toBeInTheDocument(),
  );
  await waitFor(() => expect(screen.getByText("Continue")).toBeInTheDocument());
  expect(screen.getAllByText(/^history-/)).toHaveLength(count);
  expect(calls.filter((call) => call.method === "POST")).toHaveLength(1);
  expect(repeatedHistoryRenders).toBe(0);
});
