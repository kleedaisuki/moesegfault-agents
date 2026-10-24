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
import { SessionWorkspace } from "./SessionWorkspace";
import i18n from "./i18n";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function mount(status = "idle") {
  i18n.changeLanguage("en");
  const writes: { path: string; body: unknown }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      if (init.method === "POST") {
        const body = JSON.parse(init.body as string);
        expect(body.operation_id).toMatch(/^[A-Za-z0-9_-]{1,128}$/);
        writes.push({ path, body });
        return new Response(
          JSON.stringify({ id: "committed", object: "workspace.version" }),
        );
      }
      return new Response(
        JSON.stringify({
          data: [
            {
              id: "version-1",
              name: "Known good",
              created_at_ms: 1792764000000,
              archive_bytes: 512,
            },
          ],
          has_more: false,
        }),
      );
    }),
  );
  render(
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
      <SessionWorkspace
        session={{ id: "session-1", agent_id: "agent-1", status }}
      />
    </QueryClientProvider>,
  );
  return writes;
}

describe("workspace actions", () => {
  it("reuses the same operation identity after an unknown network outcome, including reopening the dialog", async () => {
    mount();
    const bodies: unknown[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_path: string, init: RequestInit = {}) => {
        bodies.push(JSON.parse(init.body as string));
        if (bodies.length === 1) throw new TypeError("Connection lost");
        return new Response(
          JSON.stringify({ id: "version-already-committed" }),
        );
      }),
    );
    const user = userEvent.setup();
    await user.click(screen.getByText("Workspace", { exact: true }));
    await user.click(screen.getByRole("button", { name: "Persist workspace" }));
    let dialog = within(screen.getByRole("dialog"));
    await user.type(
      dialog.getByLabelText("Version name (optional)"),
      "Keep me",
    );
    await user.click(dialog.getByRole("button", { name: "Persist workspace" }));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(dialog.getByLabelText("Version name (optional)")).toBeDisabled();
    await user.click(dialog.getByRole("button", { name: "Cancel" }));
    await user.click(screen.getByRole("button", { name: "Persist workspace" }));
    dialog = within(screen.getByRole("dialog"));
    expect(dialog.getByLabelText("Version name (optional)")).toHaveValue(
      "Keep me",
    );
    await user.click(dialog.getByRole("button", { name: "Check operation" }));
    await waitFor(() => expect(bodies).toHaveLength(2));
    expect(bodies[0]).toEqual(bodies[1]);
    expect(bodies[0]).toMatchObject({
      name: "Keep me",
      operation_id: expect.stringMatching(/^[A-Za-z0-9_-]{1,128}$/),
    });
  });
  it("persists an idle workspace with an optional user-facing version name", async () => {
    const writes = mount();
    const user = userEvent.setup();
    await user.click(screen.getByText("Workspace", { exact: true }));
    expect(fetch).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Persist workspace" }));
    const dialog = within(screen.getByRole("dialog"));
    await user.type(
      dialog.getByLabelText("Version name (optional)"),
      "Before migration",
    );
    await user.click(dialog.getByRole("button", { name: "Persist workspace" }));
    await waitFor(() =>
      expect(writes).toEqual([
        {
          path: "/v1/sessions/session-1/workspace/commit",
          body: { name: "Before migration", operation_id: expect.any(String) },
        },
      ]),
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  it("loads whole-space versions on demand and requires explicit restore selection", async () => {
    const writes = mount();
    const user = userEvent.setup();
    await user.click(screen.getByText("Workspace", { exact: true }));
    await user.click(screen.getByRole("button", { name: "Restore workspace" }));
    const dialog = within(screen.getByRole("dialog"));
    const restore = dialog.getByRole("button", { name: "Restore workspace" });
    expect(restore).toBeDisabled();
    expect(
      dialog.getByText("Your current workspace is saved before restoring."),
    ).toBeInTheDocument();
    await user.click(await dialog.findByRole("radio", { name: /Known good/ }));
    await user.click(restore);
    await waitFor(() =>
      expect(writes).toEqual([
        {
          path: "/v1/sessions/session-1/workspace/restore",
          body: { version_id: "version-1", operation_id: expect.any(String) },
        },
      ]),
    );
  });
  it.each(["running", "closed", "waiting_for_tools"])(
    "does not mutate a %s workspace",
    async (status) => {
      mount(status);
      const user = userEvent.setup();
      await user.click(screen.getByText("Workspace", { exact: true }));
      expect(
        screen.getByRole("button", { name: "Persist workspace" }),
      ).toBeDisabled();
      expect(
        screen.getByRole("button", { name: "Restore workspace" }),
      ).toBeDisabled();
      expect(fetch).not.toHaveBeenCalled();
    },
  );
});
