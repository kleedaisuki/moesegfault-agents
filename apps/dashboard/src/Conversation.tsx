import { useTranslation } from "react-i18next";
import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X, Send } from "lucide-react";
import {
  request,
  ApiError,
  json,
  streamCompletion,
  type Agent,
  type Session,
} from "./api";
import { sessionPollInterval } from "./polling";
import { ErrorNotice, Empty } from "./ui";
export function Conversation({
  agent,
  back,
}: {
  agent: Agent;
  back: () => void;
}) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [sessionId, setSessionId] = useState("");
  const [resumeId, setResumeId] = useState("");
  const sessions = useQuery({
    queryKey: ["sessions", agent.id],
    queryFn: () =>
      request<{ data: Session[] }>(
        `/v1/agents/${encodeURIComponent(agent.id)}/sessions`,
      ),
  });
  const [content, setContent] = useState("");
  const [streamId, setStreamId] = useState<string | null>(null);
  const [partial, setPartial] = useState("");
  const streamController = useRef<AbortController | null>(null);
  useEffect(
    () => () => {
      streamController.current?.abort();
    },
    [sessionId],
  );
  const run = useMutation({
    mutationFn: async ({
      id,
      content,
    }: {
      id: string;
      content: string;
      historyLength: number;
    }) => {
      const controller = new AbortController();
      streamController.current = controller;
      setStreamId(id);
      setPartial("");
      setContent("");
      try {
        await streamCompletion(
          `/v1/sessions/${encodeURIComponent(id)}/messages`,
          {
            ...json("POST", {
              messages: [{ role: "user", content }],
              stream: true,
            }),
            signal: controller.signal,
          },
          (delta) => setPartial((previous) => previous + delta),
        );
      } catch (error) {
        if (
          error instanceof ApiError &&
          error.status === 409 &&
          error.code === "session_busy"
        )
          await request(
            `/v1/sessions/${encodeURIComponent(id)}/messages`,
            json("POST", { content }),
          );
        else throw error;
      }
    },
    onSettled: async (_data, _error, variables) => {
      await cache.invalidateQueries({ queryKey: ["session", variables.id] });
      setStreamId((current) => (current === variables.id ? null : current));
    },
  });
  const session = useQuery({
    queryKey: ["session", sessionId],
    enabled: !!sessionId,
    queryFn: ({ signal }) =>
      request<Session>(`/v1/sessions/${encodeURIComponent(sessionId)}`, {
        signal,
      }),
    staleTime: 5_000,
    refetchOnWindowFocus: true,
    refetchIntervalInBackground: false,
    refetchInterval: (query) =>
      streamId === sessionId
        ? false
        : sessionPollInterval(
            query.state.data,
            query.state.error,
            query.state.fetchFailureCount,
          ),
  });
  const create = useMutation({
    mutationFn: () =>
      request<Session>(
        `/v1/agents/${encodeURIComponent(agent.id)}/sessions`,
        json("POST", {}),
      ),
    onSuccess: (data) => {
      setSessionId(data.id);
      cache.setQueryData<{ data: Session[] }>(
        ["sessions", agent.id],
        (previous) => ({ data: [data, ...(previous?.data ?? [])] }),
      );
    },
  });
  const send = useMutation({
    mutationFn: () =>
      request(
        `/v1/sessions/${encodeURIComponent(sessionId)}/messages`,
        json("POST", { content }),
      ),
    onSuccess: () => {
      setContent("");
      session.refetch();
    },
  });
  const resume = useMutation({
    mutationFn: () =>
      request(
        `/v1/sessions/${encodeURIComponent(sessionId)}/resume`,
        json("POST", {}),
      ),
    onSuccess: () => {
      session.refetch();
      sessions.refetch();
    },
  });
  const end = useMutation({
    mutationFn: () =>
      request(`/v1/sessions/${encodeURIComponent(sessionId)}`, json("DELETE")),
    onSuccess: () => {
      session.refetch();
      sessions.refetch();
    },
  });
  return (
    <>
      <button className="back" onClick={back}>
        {t("← All agents")}
      </button>
      <header className="page-header">
        <div>
          <div className="eyebrow">{agent.model.model}</div>
          <h1>{agent.name}</h1>
          <p className="mono">{agent.id}</p>
        </div>
        <button onClick={() => create.mutate()} disabled={create.isPending}>
          <Plus size={16} />
          {t("New session")}
        </button>
      </header>
      <form
        className="resume"
        onSubmit={(e) => {
          e.preventDefault();
          setSessionId(resumeId.trim());
        }}
      >
        <input
          aria-label={t("Session ID")}
          placeholder={t("Resume by session ID")}
          value={resumeId}
          onChange={(e) => setResumeId(e.target.value)}
        />
        <button disabled={!resumeId.trim()}>{t("Open")}</button>
      </form>
      {!!sessions.data?.data.length && (
        <div className="session-list" aria-label={t("Sessions")}>
          {sessions.data.data.map((item) => (
            <button
              className={item.id === sessionId ? "selected" : ""}
              key={item.id}
              onClick={() => setSessionId(item.id)}
            >
              <span className="mono">{item.id.slice(0, 8)}</span>
              <small>
                {t(
                  item.id === sessionId
                    ? (session.data?.status ?? item.status ?? "Connecting")
                    : (item.status ?? "Connecting"),
                )}
              </small>
            </button>
          ))}
        </div>
      )}
      <ErrorNotice
        error={
          create.error ??
          sessions.error ??
          session.error ??
          send.error ??
          (run.error instanceof DOMException && run.error.name === "AbortError"
            ? null
            : run.error) ??
          end.error ??
          resume.error ??
          session.data?.last_error
        }
      />
      {!sessionId ? (
        <Empty title={t("No session selected")}>
          <button
            className="primary"
            onClick={() => create.mutate()}
            disabled={create.isPending}
          >
            {t("Start a session")}
          </button>
        </Empty>
      ) : (
        <section className="conversation">
          <header>
            <div>
              <span className="status-dot" />
              {t(session.data?.status ?? "Connecting")}
              <small className="mono">{sessionId}</small>
            </div>
            <button
              className="icon"
              aria-label={t("Close session")}
              disabled={end.isPending}
              onClick={() => {
                if (
                  window.confirm(
                    t("Close this session and persist its workspace?"),
                  )
                )
                  end.mutate();
              }}
            >
              <X size={18} />
            </button>
          </header>
          <div className="messages" aria-live="polite">
            {session.data?.status === "closed" && (
              <div className="notice">
                {t(
                  "Session archived. Your pinned instructions and workspace are preserved.",
                )}{" "}
                <button
                  onClick={() => resume.mutate()}
                  disabled={resume.isPending}
                >
                  {t("Resume session")}
                </button>
              </div>
            )}
            {session.data?.messages?.length ? (
              session.data.messages.map((m, i) => (
                <article className={`message ${m.role}`} key={i}>
                  <span>{t(m.role)}</span>
                  <p>
                    {typeof m.content === "string"
                      ? m.content
                      : JSON.stringify(m.content)}
                  </p>
                </article>
              ))
            ) : (
              <p className="hint">
                {t(
                  "Send a message to begin. You can add direction while your agent works.",
                )}
              </p>
            )}
            {streamId === sessionId && (
              <>
                {session.data?.messages?.[run.variables?.historyLength ?? -1]
                  ?.content !== run.variables?.content && (
                  <article className="message user">
                    <span>{t("user")}</span>
                    <p>{run.variables?.content}</p>
                  </article>
                )}
                <article className="message assistant">
                  <span>{t("assistant")}</span>
                  <p>{partial || t("Working…")}</p>
                </article>
              </>
            )}
            {session.data?.pending_messages?.map((m, i) => (
              <article className="message user pending" key={`pending-${i}`}>
                <span>{t("queued")}</span>
                <p>{m.content}</p>
              </article>
            ))}
          </div>
          <form
            className="composer"
            onSubmit={(e) => {
              e.preventDefault();
              if (session.data?.status === "running" || streamId === sessionId)
                send.mutate();
              else
                run.mutate({
                  id: sessionId,
                  content,
                  historyLength: session.data?.messages?.length ?? 0,
                });
            }}
          >
            <textarea
              aria-label={t("Message or steering instruction")}
              placeholder={t("A task, a question, a new direction…")}
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={3}
              required
            />
            <footer>
              <span>
                {session.data?.context_tokens !== undefined
                  ? t("contextTokens", { count: session.data.context_tokens })
                  : t("Instructions pinned · workspace persistent")}
              </span>
              <button
                className="primary"
                disabled={
                  !content.trim() ||
                  send.isPending ||
                  session.data?.status === "closed"
                }
              >
                <Send size={16} />
                {send.isPending ? t("Sending…") : t("Send")}
              </button>
            </footer>
          </form>
        </section>
      )}
    </>
  );
}
