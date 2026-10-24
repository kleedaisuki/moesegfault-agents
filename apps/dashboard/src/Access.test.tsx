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
import { Access } from "./Access";
import i18n from "./i18n";
import type { ProviderKey } from "./api";

const writes: {
  path: string;
  method: string;
  body?: Record<string, unknown>;
}[] = [];
function mount() {
  let credentials: ProviderKey[] = [
    { id: "old-owned-id", provider: "deepseek" },
  ];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      const method = init.method ?? "GET";
      if (method === "POST") {
        const body = JSON.parse(init.body as string);
        writes.push({ path, method, body });
        const { api_key: _secret, ...metadata } = body;
        credentials = [
          ...credentials,
          { id: `cred-${credentials.length}`, ...metadata },
        ];
        return Response.json(credentials.at(-1), { status: 201 });
      }
      if (method === "DELETE") {
        writes.push({ path, method });
        credentials = credentials.filter((key) => !path.endsWith(key.id));
        return new Response(null, { status: 204 });
      }
      return Response.json({
        data: path === "/v1/provider-keys" ? credentials : [],
      });
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
      <Access />
    </QueryClientProvider>,
  );
}
beforeEach(() => {
  writes.length = 0;
  localStorage.clear();
  sessionStorage.clear();
  i18n.changeLanguage("en");
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("protocol-based provider connections", () => {
  it.each(["openai", "anthropic"] as const)(
    "saves a named %s endpoint and never retains its secret after success",
    async (protocol) => {
      mount();
      const user = userEvent.setup();
      const select = screen.getByLabelText("API type");
      expect(
        within(select)
          .getAllByRole("option")
          .map((option) => option.getAttribute("value")),
      ).toEqual(["openai", "anthropic"]);
      await user.selectOptions(select, protocol);
      await user.type(
        screen.getByLabelText("Connection name (optional)"),
        "My endpoint",
      );
      await user.type(
        screen.getByLabelText("Base URL"),
        "https://custom.example/v1",
      );
      await user.type(
        screen.getByLabelText("Provider API key"),
        "synthetic-private-key",
      );
      await user.click(screen.getByRole("button", { name: "Save key" }));
      await waitFor(() => expect(writes).toHaveLength(1));
      expect(writes[0]).toEqual({
        path: "/v1/provider-keys",
        method: "POST",
        body: {
          protocol,
          name: "My endpoint",
          base_url: "https://custom.example/v1",
          api_key: "synthetic-private-key",
        },
      });
      await waitFor(() =>
        expect(screen.getByLabelText("Provider API key")).toHaveValue(""),
      );
      expect(
        await screen.findByText("My endpoint", { exact: true }),
      ).toBeInTheDocument();
      expect(localStorage.length).toBe(0);
      expect(sessionStorage.length).toBe(0);
      expect(document.body.textContent).not.toContain("synthetic-private-key");
    },
  );

  it("removes a legacy credential by its opaque ID rather than its vendor alias", async () => {
    mount();
    vi.spyOn(window, "confirm").mockReturnValue(true);
    await screen.findByText("deepseek", { exact: true });
    await userEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() =>
      expect(writes).toEqual([
        { path: "/v1/provider-keys/old-owned-id", method: "DELETE" },
      ]),
    );
  });
});
