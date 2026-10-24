import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  useInfiniteQuery,
  useMutation,
  useQueryClient,
} from "@tanstack/react-query";
import { History, Save } from "lucide-react";
import { ApiError, formattedDate, json, request, type Session } from "./api";
import { formattedBytes, spacePage, type WorkspaceVersion } from "./space-api";
import { Empty, ErrorNotice, Modal } from "./ui";

type WorkspaceOperation = {
  action: "commit" | "restore";
  operation_id: string;
  name?: string;
  version_id?: string;
};

export function SessionWorkspace({ session }: { session: Session }) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [action, setAction] = useState<"commit" | "restore" | null>(null);
  const operationKey = ["workspace-operation", session.id];
  const [operation, setOperation] = useState<WorkspaceOperation | null>(
    () => cache.getQueryData<WorkspaceOperation>(operationKey) ?? null,
  );
  function remember(value: WorkspaceOperation | null) {
    setOperation(value);
    if (value) cache.setQueryData(operationKey, value);
    else cache.removeQueries({ queryKey: operationKey });
  }
  const available = session.status === "idle";
  return (
    <details className="workspace-controls">
      <summary>{t("Workspace")}</summary>
      <div>
        <button
          disabled={
            !available || (!!operation && operation.action !== "commit")
          }
          onClick={() => setAction("commit")}
        >
          <Save size={16} />
          {t("Persist workspace")}
        </button>
        <button
          disabled={
            !available || (!!operation && operation.action !== "restore")
          }
          onClick={() => setAction("restore")}
        >
          <History size={16} />
          {t("Restore workspace")}
        </button>
      </div>
      {action && (
        <WorkspaceAction
          sessionId={session.id}
          action={action}
          operation={operation}
          remember={remember}
          close={() => setAction(null)}
        />
      )}
    </details>
  );
}

function WorkspaceAction({
  sessionId,
  action,
  operation,
  remember,
  close,
}: {
  sessionId: string;
  action: "commit" | "restore";
  operation: WorkspaceOperation | null;
  remember: (value: WorkspaceOperation | null) => void;
  close: () => void;
}) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [name, setName] = useState(operation?.name ?? "");
  const [versionId, setVersionId] = useState(operation?.version_id ?? "");
  const versions = useInfiniteQuery({
    queryKey: ["workspace-versions"],
    enabled: action === "restore",
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) =>
      spacePage<WorkspaceVersion>(
        "/v1/space/workspace-versions",
        pageParam,
        signal,
      ),
    getNextPageParam: (page) =>
      page.has_more ? (page.last_id ?? undefined) : undefined,
  });
  const save = useMutation({
    mutationFn: () => {
      const selected = operation ?? {
        action,
        operation_id: crypto.randomUUID(),
        ...(action === "commit"
          ? name.trim()
            ? { name: name.trim() }
            : {}
          : { version_id: versionId }),
      };
      remember(selected);
      const { action: kind, ...body } = selected;
      return request(
        `/v1/sessions/${encodeURIComponent(sessionId)}/workspace/${kind}`,
        json("POST", body),
      );
    },
    onSuccess: () => {
      void cache.invalidateQueries({ queryKey: ["session", sessionId] });
      void cache.invalidateQueries({ queryKey: ["workspace-versions"] });
      void cache.invalidateQueries({ queryKey: ["space"] });
      remember(null);
      close();
    },
    onError: (error) => {
      if (error instanceof ApiError && error.status === 400) remember(null);
    },
  });
  const data = versions.data?.pages.flatMap((page) => page.data) ?? [];
  return (
    <Modal
      title={t(action === "commit" ? "Persist workspace" : "Restore workspace")}
      close={close}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          save.mutate();
        }}
      >
        {operation && (
          <div className="notice">
            <p>
              {t(
                "Keep this operation ID. Checking again will not repeat a completed workspace action.",
              )}
            </p>
            <code className="operation-id">{operation.operation_id}</code>
          </div>
        )}
        {action === "commit" ? (
          <label>
            {t("Version name (optional)")}
            <input
              value={name}
              onChange={(event) => setName(event.target.value)}
              maxLength={100}
              autoComplete="off"
              disabled={!!operation}
            />
          </label>
        ) : (
          <>
            <p className="hint">
              {t("Your current workspace is saved before restoring.")}
            </p>
            {versions.isPending ? (
              <Empty title={t("Loading…")} />
            ) : !data.length && !versions.error ? (
              <Empty title={t("No workspace versions")} />
            ) : (
              <fieldset className="workspace-choice">
                <legend className="sr-only">{t("Workspace versions")}</legend>
                {data.map((version) => (
                  <label key={version.id} className="workspace-option">
                    <input
                      type="radio"
                      name="version"
                      value={version.id}
                      checked={versionId === version.id}
                      onChange={() => setVersionId(version.id)}
                      required
                      disabled={!!operation}
                    />
                    <span>
                      <strong>{version.name || version.id}</strong>
                      <small>
                        {formattedDate(version.created_at_ms)} ·{" "}
                        {formattedBytes(version.archive_bytes)}
                      </small>
                    </span>
                  </label>
                ))}
              </fieldset>
            )}
            {versions.hasNextPage && (
              <button
                type="button"
                className="space-more"
                disabled={versions.isFetchingNextPage}
                onClick={() => void versions.fetchNextPage()}
              >
                {t(versions.isFetchingNextPage ? "Loading…" : "Load more")}
              </button>
            )}
          </>
        )}
        <ErrorNotice error={save.error ?? versions.error} />
        <footer>
          <button type="button" onClick={close}>
            {t("Cancel")}
          </button>
          <button
            className="primary"
            disabled={save.isPending || (action === "restore" && !versionId)}
          >
            {t(
              save.isPending
                ? "Saving…"
                : operation
                  ? "Check operation"
                  : action === "commit"
                    ? "Persist workspace"
                    : "Restore workspace",
            )}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
