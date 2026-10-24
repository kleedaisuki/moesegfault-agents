import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  Download,
  FileText,
  Pencil,
  RefreshCw,
  Trash2,
  Upload,
} from "lucide-react";
import { formattedDate, json, request } from "./api";
import {
  formattedBytes,
  spacePage,
  uploadSpaceFile,
  type SpaceFile,
  type SpaceInfo,
  type WorkspaceVersion,
} from "./space-api";
import { Empty, ErrorNotice, Modal } from "./ui";

export function Space() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<"files" | "versions">("files");
  const [renaming, setRenaming] = useState(false);
  const space = useQuery({
    queryKey: ["space"],
    queryFn: ({ signal }) => request<SpaceInfo>("/v1/space", { signal }),
  });
  return (
    <>
      <header className="page-header">
        <div className="space-heading">
          <h1>{space.data?.name || t("Space")}</h1>
          <button
            className="icon"
            aria-label={t("Rename space")}
            onClick={() => setRenaming(true)}
            disabled={!space.data}
          >
            <Pencil size={18} />
          </button>
        </div>
        {space.data && (
          <span className="space-storage">
            <span className="sr-only">{t("Space storage")}: </span>
            {formattedBytes(space.data.usage.bytes)} /{" "}
            {formattedBytes(space.data.usage.limit_bytes)}
          </span>
        )}
      </header>
      <ErrorNotice error={space.error} />
      <div
        className="space-tabs"
        role="tablist"
        aria-label={t("Space contents")}
        onKeyDown={(event) => {
          if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key))
            return;
          event.preventDefault();
          const next =
            event.key === "Home"
              ? "files"
              : event.key === "End"
                ? "versions"
                : tab === "files"
                  ? "versions"
                  : "files";
          setTab(next);
          document.getElementById(`space-${next}-tab`)?.focus();
        }}
      >
        <button
          role="tab"
          id="space-files-tab"
          tabIndex={tab === "files" ? 0 : -1}
          aria-selected={tab === "files"}
          aria-controls="space-files-panel"
          onClick={() => setTab("files")}
        >
          {t("Files")}
        </button>
        <button
          role="tab"
          id="space-versions-tab"
          tabIndex={tab === "versions" ? 0 : -1}
          aria-selected={tab === "versions"}
          aria-controls="space-versions-panel"
          onClick={() => setTab("versions")}
        >
          {t("Workspace versions")}
        </button>
      </div>
      {tab === "files" ? (
        <div
          role="tabpanel"
          id="space-files-panel"
          aria-labelledby="space-files-tab"
        >
          <Files />
        </div>
      ) : (
        <div
          role="tabpanel"
          id="space-versions-panel"
          aria-labelledby="space-versions-tab"
        >
          <WorkspaceVersions />
        </div>
      )}
      {renaming && space.data && (
        <NameEditor
          title={t("Rename space")}
          value={space.data.name}
          path="/v1/space"
          field="name"
          queryKey="space"
          close={() => setRenaming(false)}
        />
      )}
    </>
  );
}

function Files() {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const picker = useRef<HTMLInputElement>(null);
  const [uploadingName, setUploadingName] = useState("");
  const [editing, setEditing] = useState<SpaceFile | null>(null);
  const files = useInfiniteQuery({
    queryKey: ["space-files"],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) =>
      spacePage<SpaceFile>("/v1/space/files", pageParam, signal),
    getNextPageParam: (page) =>
      page.has_more ? (page.last_id ?? undefined) : undefined,
  });
  const upload = useMutation({
    mutationFn: async (selected: File[]) => {
      for (const file of selected) {
        setUploadingName(file.name);
        await uploadSpaceFile(file);
      }
    },
    onSettled: () => {
      setUploadingName("");
      void cache.invalidateQueries({ queryKey: ["space-files"] });
      void cache.invalidateQueries({ queryKey: ["space"] });
    },
  });
  const remove = useMutation({
    mutationFn: (id: string) =>
      request(`/v1/space/files/${encodeURIComponent(id)}`, json("DELETE")),
    onSuccess: () => {
      void cache.invalidateQueries({ queryKey: ["space-files"] });
      void cache.invalidateQueries({ queryKey: ["space"] });
    },
  });
  const data = files.data?.pages.flatMap((page) => page.data) ?? [];
  return (
    <>
      <div className="space-toolbar">
        <input
          ref={picker}
          className="file-picker"
          type="file"
          multiple
          aria-label={t("Choose files")}
          onChange={(event) => {
            const selected = [...(event.target.files ?? [])];
            event.target.value = "";
            if (selected.length) upload.mutate(selected);
          }}
        />
        <button
          className="primary"
          onClick={() => picker.current?.click()}
          disabled={upload.isPending}
        >
          <Upload size={17} />
          {t(upload.isPending ? "Uploading…" : "Upload files")}
        </button>
        <button
          className="icon"
          aria-label={t("Refresh files")}
          onClick={() => void files.refetch()}
          disabled={files.isFetching}
        >
          <RefreshCw size={17} />
        </button>
      </div>
      {upload.isPending && (
        <p role="status" className="hint">
          {t("Uploading {{name}}…", { name: uploadingName })}
        </p>
      )}
      <ErrorNotice error={files.error ?? upload.error ?? remove.error} />
      {files.isPending ? (
        <Empty title={t("Loading…")} />
      ) : data.length ? (
        <section className="panel space-list" aria-label={t("Files")}>
          {data.map((file) => (
            <div className="space-row" key={file.id}>
              <FileText
                size={20}
                className="space-file-icon"
                aria-hidden="true"
              />
              <div className="space-item">
                <strong>{file.filename}</strong>
                <span>
                  {formattedBytes(file.bytes)}
                  <span aria-hidden="true"> · </span>
                  {formattedDate(file.created_at)}
                </span>
              </div>
              <div className="space-actions">
                <a
                  className="icon button"
                  href={`/v1/space/files/${encodeURIComponent(file.id)}/content`}
                  download
                  aria-label={t("Download {{name}}", { name: file.filename })}
                >
                  <Download size={18} />
                </a>
                <button
                  className="icon"
                  aria-label={t("Rename {{name}}", { name: file.filename })}
                  onClick={() => setEditing(file)}
                >
                  <Pencil size={17} />
                </button>
                <button
                  className="icon danger"
                  aria-label={t("Delete {{name}}", { name: file.filename })}
                  disabled={remove.isPending}
                  onClick={() => {
                    if (
                      window.confirm(
                        t("Delete {{name}} from your space?", {
                          name: file.filename,
                        }),
                      )
                    )
                      remove.mutate(file.id);
                  }}
                >
                  <Trash2 size={17} />
                </button>
              </div>
            </div>
          ))}
        </section>
      ) : (
        !files.error && <Empty title={t("No files")} />
      )}
      {files.hasNextPage && (
        <button
          className="space-more"
          disabled={files.isFetchingNextPage}
          onClick={() => void files.fetchNextPage()}
        >
          {t(files.isFetchingNextPage ? "Loading…" : "Load more")}
        </button>
      )}
      {editing && (
        <NameEditor
          title={t("Rename file")}
          value={editing.filename}
          path={`/v1/space/files/${encodeURIComponent(editing.id)}`}
          field="filename"
          queryKey="space-files"
          close={() => setEditing(null)}
        />
      )}
    </>
  );
}

function WorkspaceVersions() {
  const { t } = useTranslation();
  const versions = useInfiniteQuery({
    queryKey: ["workspace-versions"],
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
  const data = versions.data?.pages.flatMap((page) => page.data) ?? [];
  return (
    <>
      <div className="space-toolbar">
        <p className="hint">
          {t("Saved by your agents. Versions are read-only.")}
        </p>
        <button
          className="icon"
          aria-label={t("Refresh workspace versions")}
          onClick={() => void versions.refetch()}
          disabled={versions.isFetching}
        >
          <RefreshCw size={17} />
        </button>
      </div>
      <ErrorNotice error={versions.error} />
      {versions.isPending ? (
        <Empty title={t("Loading…")} />
      ) : data.length ? (
        <section
          className="panel space-list"
          aria-label={t("Workspace versions")}
        >
          {data.map((version) => (
            <div className="space-row" key={version.id}>
              <FileText
                size={20}
                className="space-file-icon"
                aria-hidden="true"
              />
              <div className="space-item">
                <strong>{version.name || version.id}</strong>
                <span>
                  {formattedBytes(version.archive_bytes)}
                  <span aria-hidden="true"> · </span>
                  {formattedDate(version.created_at_ms)}
                </span>
              </div>
              <a
                className="icon button"
                href={`/v1/space/workspace-versions/${encodeURIComponent(version.id)}/content`}
                download
                aria-label={t("Download {{name}}", {
                  name: version.name || version.id,
                })}
              >
                <Download size={18} />
              </a>
            </div>
          ))}
        </section>
      ) : (
        !versions.error && <Empty title={t("No workspace versions")} />
      )}
      {versions.hasNextPage && (
        <button
          className="space-more"
          disabled={versions.isFetchingNextPage}
          onClick={() => void versions.fetchNextPage()}
        >
          {t(versions.isFetchingNextPage ? "Loading…" : "Load more")}
        </button>
      )}
    </>
  );
}

function NameEditor({
  title,
  value,
  path,
  field,
  queryKey,
  close,
}: {
  title: string;
  value: string;
  path: string;
  field: "name" | "filename";
  queryKey: string;
  close: () => void;
}) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [name, setName] = useState(value);
  const save = useMutation({
    mutationFn: () => request(path, json("PATCH", { [field]: name.trim() })),
    onSuccess: () => {
      void cache.invalidateQueries({ queryKey: [queryKey] });
      close();
    },
  });
  return (
    <Modal title={title} close={close}>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          save.mutate();
        }}
      >
        <label>
          {t("Name")}
          <input
            required
            maxLength={field === "filename" ? 255 : 100}
            value={name}
            onChange={(event) => setName(event.target.value)}
            autoComplete="off"
          />
        </label>
        <ErrorNotice error={save.error} />
        <footer>
          <button type="button" onClick={close}>
            {t("Cancel")}
          </button>
          <button className="primary" disabled={!name.trim() || save.isPending}>
            {t(save.isPending ? "Saving…" : "Save")}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
