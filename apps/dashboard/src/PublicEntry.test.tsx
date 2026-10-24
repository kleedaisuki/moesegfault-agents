import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { setCsrfToken } from "./api";
import i18n from "./i18n";

function mount() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <App />
    </QueryClientProvider>,
  );
  return client;
}
beforeEach(() => {
  window.history.replaceState(null, "", "/");
  setCsrfToken(undefined);
  i18n.changeLanguage("en");
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("public authentication boundary", () => {
  it("never mounts private navigation or fetches agent data before session verification", async () => {
    let resolve!: (value: Response) => void;
    const fetch = vi.fn(
      (_path: string) =>
        new Promise<Response>((done) => {
          resolve = done;
        }),
    );
    vi.stubGlobal("fetch", fetch);
    mount();
    expect(screen.getByRole("status")).toHaveTextContent(
      "Checking your session",
    );
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "New agent" }),
    ).not.toBeInTheDocument();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe("/v1/account");
    resolve(new Response('{"error":{"message":"Sign in"}}', { status: 401 }));
    expect(
      await screen.findByRole("link", { name: "Continue with moeSegFault" }),
    ).toHaveAttribute("href", "/auth/login?return_to=/");
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
      "Your agents.",
    );
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("keeps connection failures outside the dashboard and retries authentication", async () => {
    let attempts = 0;
    const fetch = vi.fn(async (path: string) => {
      if (path === "/v1/account")
        return ++attempts === 1
          ? new Response('{"error":{"message":"Temporary outage"}}', {
              status: 503,
            })
          : Response.json({ account_id: "owned", plan: "free" });
      return Response.json({ data: [] });
    });
    vi.stubGlobal("fetch", fetch);
    mount();
    expect(
      await screen.findByRole("heading", { name: "Let’s reconnect." }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(fetch).toHaveBeenCalledTimes(1);
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(
      await screen.findByRole("navigation", { name: "Main navigation" }),
    ).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("removes the authenticated shell and private cache after a management request expires", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (path: string) => {
        if (path === "/v1/account")
          return Response.json({
            account_id: "owned",
            plan: "free",
            csrf_token: "private-csrf",
          });
        if (path === "/v1/skills")
          return new Response('{"error":{"message":"Expired"}}', {
            status: 401,
          });
        return Response.json({ data: [] });
      }),
    );
    const client = mount();
    await userEvent.click(
      await screen.findByRole("button", { name: "Skills" }),
    );
    const login = await screen.findByRole("link", {
      name: "Continue with moeSegFault",
    });
    expect(login).toHaveAttribute("href", "/auth/login?return_to=/%23skills");
    expect(screen.getByRole("status")).toHaveTextContent(
      "Your session has ended",
    );
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    await waitFor(() =>
      expect(client.getQueryData(["agents"])).toBeUndefined(),
    );
    expect(client.getQueryData(["skills"])).toBeUndefined();
    expect(client.getQueryData(["account"])).toBeNull();
  });

  it("preserves only known management return destinations, never a URL supplied in the fragment", async () => {
    window.history.replaceState(null, "", "/#https://untrusted.example/");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("{}", { status: 401 })),
    );
    mount();
    expect(
      await screen.findByRole("link", { name: "Continue with moeSegFault" }),
    ).toHaveAttribute("href", "/auth/login?return_to=/");
  });
});
