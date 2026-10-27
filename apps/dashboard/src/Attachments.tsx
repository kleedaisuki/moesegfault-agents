import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  useInfiniteQuery,
  useMutation,
  useQueryClient,
} from "@tanstack/react-query";
import { Upload } from "lucide-react";
import {
  formattedBytes,
  spacePage,
  uploadSpaceFile,
  type SpaceFile,
} from "./space-api";
import { Empty, ErrorNotice, Modal } from "./ui";

export type Attachment = Pick<SpaceFile, "id" | "filename" | "content_type">;

export function Attachments({
  selected,
  apply,
  close,
}: {
  selected: Attachment[];
  apply: (files: Attachment[]) => void;
  close: () => void;
}) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const picker = useRef<HTMLInputElement>(null);
  const [chosen, setChosen] = useState(selected);
  const files = useInfiniteQuery({
    queryKey: ["space-files"],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) =>
      spacePage<SpaceFile>("/v1/space/files", pageParam, signal),
    getNextPageParam: (page) =>
      page.has_more ? (page.last_id ?? undefined) : undefined,
  });
  const upload = useMutation({
    mutationFn: async (file: File) => uploadSpaceFile(file),
    onSuccess: (file) => {
      setChosen((previous) => [
        ...previous,
        {
          id: file.id,
          filename: file.filename,
          content_type: file.content_type,
        },
      ]);
      void cache.invalidateQueries({ queryKey: ["space-files"] });
      void cache.invalidateQueries({ queryKey: ["space"] });
    },
  });
  const data = files.data?.pages.flatMap((page) => page.data) ?? [];
  return (
    <Modal title={t("Attach files")} close={close}>
      <div className="space-toolbar">
        <input
          ref={picker}
          className="file-picker"
          type="file"
          aria-label={t("Upload attachment")}
          onChange={(event) => {
            const file = event.target.files?.[0];
            event.target.value = "";
            if (file) upload.mutate(file);
          }}
        />
        <button
          type="button"
          onClick={() => picker.current?.click()}
          disabled={upload.isPending}
        >
          <Upload size={17} />
          {t(upload.isPending ? "Uploading…" : "Upload file")}
        </button>
      </div>
      <ErrorNotice error={files.error ?? upload.error} />
      {files.isPending ? (
        <Empty title={t("Loading…")} />
      ) : data.length ? (
        <fieldset className="workspace-choice">
          <legend className="sr-only">{t("Files")}</legend>
          {data.map((file) => (
            <label className="workspace-option" key={file.id}>
              <input
                type="checkbox"
                checked={chosen.some((value) => value.id === file.id)}
                onChange={(event) =>
                  setChosen((previous) =>
                    event.target.checked
                      ? [
                          ...previous,
                          {
                            id: file.id,
                            filename: file.filename,
                            content_type: file.content_type,
                          },
                        ]
                      : previous.filter((value) => value.id !== file.id),
                  )
                }
              />
              <span>
                <strong>{file.filename}</strong>
                <small>{formattedBytes(file.bytes)}</small>
              </span>
            </label>
          ))}
        </fieldset>
      ) : (
        !files.error && <Empty title={t("No files")} />
      )}
      {files.hasNextPage && (
        <button
          type="button"
          className="space-more"
          disabled={files.isFetchingNextPage}
          onClick={() => void files.fetchNextPage()}
        >
          {t(files.isFetchingNextPage ? "Loading…" : "Load more")}
        </button>
      )}
      <footer>
        <button type="button" onClick={close}>
          {t("Cancel")}
        </button>
        <button
          className="primary"
          type="button"
          disabled={upload.isPending}
          onClick={() => {
            apply(chosen);
            close();
          }}
        >
          {t("Attach selected")}
        </button>
      </footer>
    </Modal>
  );
}
