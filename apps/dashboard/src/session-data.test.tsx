import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  mergeJournal,
  sessionView,
  useSessionJournal,
  type JournalPage,
} from "./session-data";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function page(start: number, end: number, high_water = end): JournalPage {
  return {
    object: "list",
    data: Array.from({ length: end - start }, (_, index) => ({
      sequence: start + index,
      message: { role: "user", content: `message-${start + index}` },
    })),
    start,
    end,
    high_water,
    before_cursor: `before-${start}`,
    after_cursor: `after-${end}`,
    has_older: start > 0,
    has_newer: end < high_water,
  };
}

describe("bandwidth-efficient session reads", () => {
  it("uses conditional metadata reads and preserves the cached snapshot on 304", async () => {
    const previous = {
      session: { id: "session-1", agent_id: "agent-1", message_count: 500 },
      etag: '"revision-7"',
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (path: string, init: RequestInit) => {
        expect(path).toBe("/v1/sessions/session-1/view");
        expect(new Headers(init.headers).get("if-none-match")).toBe(
          '"revision-7"',
        );
        return new Response(null, { status: 304 });
      }),
    );
    expect(
      await sessionView("session-1", previous, new AbortController().signal),
    ).toBe(previous);
  });
  it("fetches only append deltas and preserves older pages loaded during a fetch", async () => {
    const calls: string[] = [];
    let append!: (response: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (path: string) => {
        calls.push(path);
        if (path.endsWith("after=after-100"))
          return new Promise<Response>((resolve) => {
            append = resolve;
          });
        return new Response(
          JSON.stringify(
            path.includes("before=") ? page(0, 50, 102) : page(50, 100),
          ),
        );
      }),
    );
    const cache = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });
    const { result, rerender } = renderHook(
      ({ count }) => useSessionJournal("session-1", count),
      {
        initialProps: { count: 100 },
        wrapper: ({ children }) => (
          <QueryClientProvider client={cache}>{children}</QueryClientProvider>
        ),
      },
    );
    await waitFor(() => expect(result.current.data?.end).toBe(100));
    rerender({ count: 102 });
    await waitFor(() => expect(append).toBeDefined());
    await act(async () => {
      result.current.loadOlder();
    });
    await waitFor(() => expect(result.current.data?.start).toBe(0));
    await act(async () => {
      append(new Response(JSON.stringify(page(100, 102))));
    });
    await waitFor(() => expect(result.current.data?.end).toBe(102));
    expect(result.current.data?.data.map((entry) => entry.sequence)).toEqual(
      Array.from({ length: 102 }, (_, index) => index),
    );
    expect(calls).toEqual([
      "/v1/sessions/session-1/messages?limit=50",
      "/v1/sessions/session-1/messages?limit=50&after=after-100",
      "/v1/sessions/session-1/messages?limit=50&before=before-50",
    ]);
  });
  it("does not refetch an unchanged journal on metadata-only rerenders", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify(page(0, 2)))),
    );
    const { result, rerender } = renderHook(
      ({ count }) => useSessionJournal("session-1", count),
      {
        initialProps: { count: 2 },
        wrapper: ({ children }) => (
          <QueryClientProvider
            client={
              new QueryClient({ defaultOptions: { queries: { retry: false } } })
            }
          >
            {children}
          </QueryClientProvider>
        ),
      },
    );
    await waitFor(() => expect(result.current.data?.end).toBe(2));
    rerender({ count: 2 });
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1));
  });
});

describe("stable journal windows", () => {
  it("merges overlap, prepend and append without losing or duplicating source entries", () => {
    for (let split = 1; split < 100; split++) {
      let current = page(split, 100, 110);
      current = mergeJournal(current, page(100, 110));
      current = mergeJournal(current, page(0, split, 100));
      current = mergeJournal(current, page(50, 100, 100));
      expect(current.data.map((entry) => entry.sequence)).toEqual(
        Array.from({ length: 110 }, (_, index) => index),
      );
      expect(current.start).toBe(0);
      expect(current.end).toBe(110);
      expect(current.high_water).toBe(110);
      expect(current.has_older).toBe(false);
      expect(current.has_newer).toBe(false);
      expect(current.after_cursor).toBe("after-110");
    }
  });
  it("rejects noncontiguous pages rather than silently dropping history", () => {
    expect(() => mergeJournal(page(0, 10), page(11, 20))).toThrow(
      "Session history gap",
    );
  });
});
