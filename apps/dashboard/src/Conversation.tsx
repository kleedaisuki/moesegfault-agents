import { useTranslation } from "react-i18next";
import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X, Send, Paperclip, RefreshCw } from "lucide-react";
import { request, json, type Agent, type Session } from "./api";
import { sessionPollInterval } from "./polling";
import { ErrorNotice, Empty } from "./ui";
import { SessionWorkspace } from "./SessionWorkspace";
import {
  sessionView,
  useSessionJournal,
  type SessionView,
} from "./session-data";
import { Attachments, type Attachment } from "./Attachments";
import { streamResponse } from "./responses-stream";
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
  const [instructions, setInstructions] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [choosingFiles, setChoosingFiles] = useState(false);
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
      files,
      instructions,
      previousResponseId,
    }: {
      id: string;
      content: string;
      files: Attachment[];
      instructions: string;
      previousResponseId?: string | null;
      historyEnd: number;
    }) => {
      const controller = new AbortController();
      streamController.current = controller;
      setStreamId(id);
      setPartial("");
      setContent("");
      setAttachments([]);
      return streamResponse(
        {
          ...json("POST", {
            model: agent.id,
            input: [
              {
                role: "user",
                content: [
                  ...(content.trim()
                    ? [{ type: "input_text", text: content }]
                    : []),
                  ...files.map((file) => ({
                    type: "input_file",
                    file_id: file.id,
                  })),
                ],
              },
            ],
            ...(previousResponseId
              ? { previous_response_id: previousResponseId }
              : {}),
            ...(instructions.trim() ? { instructions } : {}),
            stream: true,
          }),
          headers: { "X-Agents-Session": id },
          signal: controller.signal,
        },
        (delta) => setPartial((previous) => previous + delta),
      );
    },
    onSettled: async (_data, _error, variables) => {
      await Promise.all([
        cache.invalidateQueries({ queryKey: ["session", variables.id] }),
        cache.invalidateQueries({
          queryKey: ["session-messages", variables.id],
        }),
      ]);
      setStreamId((current) => (current === variables.id ? null : current));
    },
  });
  const session = useQuery({
    queryKey: ["session", sessionId],
    enabled: !!sessionId,
    queryFn: ({ signal }) =>
      sessionView(
        sessionId,
        cache.getQueryData<SessionView>(["session", sessionId]),
        signal,
      ),
    select: (data) => data.session,
    staleTime: 5_000,
    refetchOnWindowFocus: true,
    refetchIntervalInBackground: false,
    refetchInterval: (query) =>
      streamId === sessionId
        ? false
        : sessionPollInterval(
            query.state.data?.session,
            query.state.error,
            query.state.fetchFailureCount,
          ),
  });
  const journal = useSessionJournal(sessionId, session.data?.message_count);
  const active = session.data?.status === "running" || streamId === sessionId;
  const awaitingTools = !!session.data?.pending_local_calls?.length;
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
          journal.error ??
          journal.olderError ??
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
            <div className="conversation-actions">
              <button
                className="icon"
                aria-label={t("Refresh conversation")}
                disabled={session.isFetching || journal.isFetching}
                onClick={() => {
                  void session.refetch();
                  void journal.refetch();
                }}
              >
                <RefreshCw size={17} />
              </button>
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
            </div>
          </header>
          {agent.workspace && session.data && (
            <SessionWorkspace key={sessionId} session={session.data} />
          )}
          <div className="messages" aria-live="polite">
            {awaitingTools && (
              <div className="notice">
                <strong>{t("Waiting for local tools")}</strong>
                <p>
                  {t(
                    "Return these function results from the client that started this response.",
                  )}
                </p>
                <ul>
                  {session.data?.pending_local_calls?.map((call) => (
                    <li key={call.call_id}>
                      <code>{call.name}</code>
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {journal.data?.has_older && (
              <button
                className="history-more"
                disabled={journal.loadingOlder}
                onClick={() => journal.loadOlder()}
              >
                {t(journal.loadingOlder ? "Loading…" : "Load earlier messages")}
              </button>
            )}
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
            {journal.data?.data.length ? (
              journal.data.data.map(({ message: m, sequence }) => (
                <article className={`message ${m.role}`} key={sequence}>
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
                {journal.data?.data.find(
                  (entry) => entry.sequence === run.variables?.historyEnd,
                )?.message.content !== run.variables?.content && (
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
              if (awaitingTools || (active && attachments.length)) return;
              if (active) send.mutate();
              else
                run.mutate({
                  id: sessionId,
                  content,
                  files: attachments,
                  instructions,
                  previousResponseId: session.data?.latest_response_id,
                  historyEnd: journal.data?.end ?? 0,
                });
            }}
          >
            <textarea
              aria-label={t("Message or steering instruction")}
              placeholder={t("A task, a question, a new direction…")}
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={3}
              required={!attachments.length}
              disabled={awaitingTools}
            />
            {!!attachments.length && (
              <div className="attachment-chips">
                {attachments.map((file) => (
                  <span key={file.id}>
                    {file.filename}
                    <button
                      type="button"
                      className="icon"
                      aria-label={t("Remove {{name}}", { name: file.filename })}
                      onClick={() =>
                        setAttachments((previous) =>
                          previous.filter((value) => value.id !== file.id),
                        )
                      }
                    >
                      <X size={15} />
                    </button>
                  </span>
                ))}
              </div>
            )}
            <details className="response-instructions">
              <summary>{t("Response instructions")}</summary>
              <label className="sr-only" htmlFor="response-instructions">
                {t("Response instructions")}
              </label>
              <textarea
                id="response-instructions"
                value={instructions}
                onChange={(event) => setInstructions(event.target.value)}
                rows={3}
                disabled={active || awaitingTools}
              />
              <p className="hint">
                {t(
                  "Kept through compaction for this response. Your agent instructions stay pinned.",
                )}
              </p>
            </details>
            <footer>
              <span>
                {session.data?.context_tokens !== undefined
                  ? t("contextTokens", { count: session.data.context_tokens })
                  : t("Instructions pinned · workspace persistent")}
              </span>
              <div className="composer-actions">
                <button
                  type="button"
                  className="icon"
                  aria-label={t("Attach files")}
                  disabled={
                    active || awaitingTools || session.data?.status === "closed"
                  }
                  onClick={() => setChoosingFiles(true)}
                >
                  <Paperclip size={18} />
                </button>
                <button
                  className="primary"
                  disabled={
                    (!content.trim() && !attachments.length) ||
                    send.isPending ||
                    awaitingTools ||
                    (active && !!attachments.length) ||
                    session.data?.status === "closed"
                  }
                >
                  <Send size={16} />
                  {send.isPending ? t("Sending…") : t("Send")}
                </button>
              </div>
            </footer>
          </form>
        </section>
      )}
      {choosingFiles && (
        <Attachments
          selected={attachments}
          apply={setAttachments}
          close={() => setChoosingFiles(false)}
        />
      )}
    </>
  );
}
