import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Conversation } from "./Conversation";
import i18n from "./i18n";
import { setCsrfToken, type Agent } from "./api";

const agent: Agent = {
  id: "agent-1",
  name: "Notebook",
  model: {
    provider: "deepseek",
    model: "flash",
    context_window: 100000,
    max_output_tokens: 8192,
  },
  instructions: "Agent instructions",
  compaction_percent: 80,
  workspace: true,
  skills: [],
};
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  setCsrfToken(undefined);
});

function mount(options: { pending?: boolean; rejected?: boolean } = {}) {
  i18n.changeLanguage("en");
  setCsrfToken("test-csrf");
  const writes: {
    path: string;
    body: Record<string, unknown>;
    headers: Headers;
  }[] = [];
  let completed = false;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      if (init.method === "POST") {
        writes.push({
          path,
          body: JSON.parse(init.body as string),
          headers: new Headers(init.headers),
        });
        if (options.rejected)
          return new Response(
            JSON.stringify({
              error: {
                message: "Session changed; inspect current state",
                code: "response_fork_not_supported",
              },
            }),
            { status: 409 },
          );
        completed = true;
        return new Response(
          `data: ${JSON.stringify({ type: "response.output_text.delta", delta: "Done." })}\n\ndata: ${JSON.stringify({ type: "response.completed", response: { id: "resp-new", status: "completed", output: [] } })}\n\n`,
          { headers: { "Content-Type": "text/event-stream" } },
        );
      }
      let data: unknown;
      if (path === "/v1/agents/agent-1/sessions")
        data = {
          data: [{ id: "session-1", agent_id: agent.id, status: "idle" }],
        };
      else if (path === "/v1/sessions/session-1/view")
        data = {
          id: "session-1",
          agent_id: agent.id,
          status: "idle",
          message_count: completed ? 2 : 0,
          latest_response_id: completed ? "resp-new" : "resp-parent",
          pending_local_calls: options.pending
            ? [{ call_id: "call-1", name: "local_lookup", arguments: "{}" }]
            : [],
        };
      else if (path.startsWith("/v1/sessions/session-1/messages?"))
        data = {
          object: "list",
          data: completed
            ? [
                {
                  sequence: 0,
                  message: { role: "user", content: "Read the brief" },
                },
                {
                  sequence: 1,
                  message: { role: "assistant", content: "Done." },
                },
              ]
            : [],
          start: 0,
          end: completed ? 2 : 0,
          high_water: completed ? 2 : 0,
          before_cursor: "before-0",
          after_cursor: completed ? "after-2" : "after-0",
          has_older: false,
          has_newer: false,
        };
      else if (path === "/v1/space/files?limit=20")
        data = {
          data: [{ id: "file-brief", filename: "brief.md", bytes: 120 }],
          has_more: false,
        };
      return new Response(
        JSON.stringify(
          data ?? { error: { message: `Unexpected route ${path}` } },
        ),
        { status: data ? 200 : 404 },
      );
    }),
  );
  render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: {
            queries: { retry: false, staleTime: 60000 },
            mutations: { retry: false },
          },
        })
      }
    >
      <Conversation agent={agent} back={() => {}} />
    </QueryClientProvider>,
  );
  return writes;
}

describe("Responses conversation", () => {
  it("submits Space file references and custom instructions to the current response head", async () => {
    const writes = mount();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /session-/ }));
    await user.click(
      await screen.findByRole("button", { name: "Attach files" }),
    );
    const dialog = within(screen.getByRole("dialog"));
    await user.click(await dialog.findByRole("checkbox", { name: /brief.md/ }));
    await user.click(dialog.getByRole("button", { name: "Attach selected" }));
    expect(
      screen.getByRole("button", { name: "Remove brief.md" }),
    ).toBeInTheDocument();
    await user.click(
      screen.getByText("Response instructions", { selector: "summary" }),
    );
    await user.type(
      screen.getByRole("textbox", { name: "Response instructions" }),
      "Be concise",
    );
    await user.type(
      screen.getByRole("textbox", { name: "Message or steering instruction" }),
      "Read the brief",
    );
    await user.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].path).toBe("/v1/responses");
    expect(writes[0].headers.get("x-agents-session")).toBe("session-1");
    expect(writes[0].headers.get("x-csrf-token")).toBe("test-csrf");
    expect(writes[0].body).toEqual({
      model: "agent-1",
      input: [
        {
          role: "user",
          content: [
            { type: "input_text", text: "Read the brief" },
            { type: "input_file", file_id: "file-brief" },
          ],
        },
      ],
      previous_response_id: "resp-parent",
      instructions: "Be concise",
      stream: true,
    });
    expect(await screen.findByText("Done.")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Remove brief.md" }),
    ).not.toBeInTheDocument();
    expect(JSON.stringify(writes)).not.toContain("Agent instructions");
  });
  it("shows pending local functions without executing or accepting ordinary user input", async () => {
    mount({ pending: true });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /session-/ }));
    expect(
      await screen.findByText("Waiting for local tools"),
    ).toBeInTheDocument();
    expect(screen.getByText("local_lookup")).toBeInTheDocument();
    expect(
      screen.getByRole("textbox", { name: "Message or steering instruction" }),
    ).toBeDisabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });
  it("does not silently retry or turn a rejected Responses request into steering", async () => {
    const writes = mount({ rejected: true });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /session-/ }));
    await user.type(
      screen.getByRole("textbox", { name: "Message or steering instruction" }),
      "A task",
    );
    await user.click(screen.getByRole("button", { name: "Send" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Session changed",
    );
    expect(writes.map((write) => write.path)).toEqual(["/v1/responses"]);
  });
});
