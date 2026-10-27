import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { request, responseFor, type Message, type Session } from "./api";

export type SessionView = { session: Session; etag?: string };
export type JournalEntry = { sequence: number; message: Message };
export type JournalPage = {
  object: "list";
  data: JournalEntry[];
  start: number;
  end: number;
  high_water: number;
  before_cursor: string;
  after_cursor: string;
  has_older: boolean;
  has_newer: boolean;
};

export async function sessionView(
  id: string,
  previous: SessionView | undefined,
  signal: AbortSignal,
): Promise<SessionView> {
  const response = await responseFor(
    `/v1/sessions/${encodeURIComponent(id)}/view`,
    {
      signal,
      headers: previous?.etag ? { "If-None-Match": previous.etag } : undefined,
    },
    [304],
  );
  if (response.status === 304) {
    if (!previous) throw new Error("Invalid session response");
    return previous;
  }
  return {
    session: (await response.json()) as Session,
    etag: response.headers.get("etag") ?? undefined,
  };
}

function messagePage(
  id: string,
  direction?: "before" | "after",
  cursor?: string,
  signal?: AbortSignal,
): Promise<JournalPage> {
  const query = new URLSearchParams({ limit: "50" });
  if (direction && cursor) query.set(direction, cursor);
  return request(`/v1/sessions/${encodeURIComponent(id)}/messages?${query}`, {
    signal,
  });
}

export function mergeJournal(
  current: JournalPage | undefined,
  page: JournalPage,
): JournalPage {
  if (!current) return page;
  if (page.start > current.end || page.end < current.start)
    throw new Error("Session history gap");
  const entries = new Map(current.data.map((entry) => [entry.sequence, entry]));
  for (const entry of page.data) entries.set(entry.sequence, entry);
  const first = page.start <= current.start ? page : current;
  const last = page.end >= current.end ? page : current;
  return {
    object: "list",
    data: [...entries.values()].sort(
      (left, right) => left.sequence - right.sequence,
    ),
    start: Math.min(current.start, page.start),
    end: Math.max(current.end, page.end),
    high_water: Math.max(current.high_water, page.high_water),
    before_cursor: first.before_cursor,
    after_cursor: last.after_cursor,
    has_older: first.has_older,
    has_newer: last.has_newer,
  };
}

export function useSessionJournal(id: string, expectedCount?: number) {
  const cache = useQueryClient();
  const key = ["session-messages", id];
  const journal = useQuery({
    queryKey: key,
    enabled: !!id,
    queryFn: async ({ signal }) => {
      const previous = cache.getQueryData<JournalPage>(key);
      const page = await messagePage(
        id,
        previous ? "after" : undefined,
        previous?.after_cursor,
        signal,
      );
      return mergeJournal(cache.getQueryData<JournalPage>(key), page);
    },
    staleTime: Infinity,
    refetchOnWindowFocus: false,
    retry: false,
  });
  useEffect(() => {
    if (!id || !journal.data || journal.isFetching || journal.error) return;
    if (
      journal.data.has_newer ||
      (expectedCount !== undefined && expectedCount > journal.data.end)
    )
      void journal.refetch();
  }, [
    id,
    expectedCount,
    journal.data,
    journal.isFetching,
    journal.error,
    journal.refetch,
  ]);
  const older = useMutation({
    mutationFn: async () => {
      const current = cache.getQueryData<JournalPage>(key);
      if (!current?.has_older) return;
      const page = await messagePage(id, "before", current.before_cursor);
      cache.setQueryData<JournalPage>(key, (latest) =>
        mergeJournal(latest, page),
      );
    },
  });
  return {
    ...journal,
    loadOlder: older.mutate,
    loadingOlder: older.isPending,
    olderError: older.error,
  };
}
