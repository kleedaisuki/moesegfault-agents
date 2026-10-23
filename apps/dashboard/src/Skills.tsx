import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, Pencil, Trash2 } from "lucide-react";
import { request, json, type Skill } from "./api";
import { ErrorNotice, Empty, Modal } from "./ui";
export function Skills() {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const skills = useQuery({
    queryKey: ["skills"],
    queryFn: () => request<{ data: Skill[] }>("/v1/skills"),
    staleTime: 300_000,
  });
  const [editing, setEditing] = useState<Skill | "new" | null>(null);
  const remove = useMutation({
    mutationFn: (name: string) =>
      request(`/v1/skills/${encodeURIComponent(name)}`, json("DELETE")),
    onSuccess: () => cache.invalidateQueries({ queryKey: ["skills"] }),
  });
  return (
    <>
      <header className="page-header">
        <h1>{t("Skills")}</h1>
        <button className="primary" onClick={() => setEditing("new")}>
          <Plus size={17} />
          {t("New skill")}
        </button>
      </header>
      <ErrorNotice error={skills.error ?? remove.error} />
      {skills.isPending ? (
        <Empty title={t("Loading…")} />
      ) : skills.data?.data.length ? (
        <section className="panel">
          {skills.data.data.map((skill) => (
            <div className="list-row skill-row" key={skill.name}>
              <strong>{skill.name}</strong>
              <div>
                <button
                  className="icon"
                  aria-label={t(
                    skill.source === "deployment"
                      ? "Customize {{name}}"
                      : "Edit {{name}}",
                    { name: skill.name },
                  )}
                  onClick={() => setEditing(skill)}
                >
                  <Pencil size={17} />
                </button>
                <button
                  className="icon danger"
                  aria-label={t("Delete {{name}}", { name: skill.name })}
                  disabled={remove.isPending || skill.source === "deployment"}
                  onClick={() => {
                    if (
                      window.confirm(
                        t(
                          "Delete {{name}}? Existing sessions keep their pinned version.",
                          { name: skill.name },
                        ),
                      )
                    )
                      remove.mutate(skill.name);
                  }}
                >
                  <Trash2 size={17} />
                </button>
              </div>
            </div>
          ))}
        </section>
      ) : (
        !skills.error && <Empty title={t("No skills")} />
      )}{" "}
      {editing && (
        <SkillEditor
          skill={editing === "new" ? undefined : editing}
          close={() => setEditing(null)}
        />
      )}
    </>
  );
}
function SkillEditor({ skill, close }: { skill?: Skill; close: () => void }) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [name, setName] = useState(skill?.name ?? "");
  const [instructions, setInstructions] = useState(skill?.instructions ?? "");
  const save = useMutation({
    mutationFn: () => {
      if (new TextEncoder().encode(instructions).byteLength > 49152)
        throw new Error("Instructions exceed 48 KiB.");
      return request(
        skill && skill.source !== "deployment"
          ? `/v1/skills/${encodeURIComponent(skill.name)}`
          : "/v1/skills",
        json(
          skill && skill.source !== "deployment" ? "PUT" : "POST",
          skill && skill.source !== "deployment"
            ? { instructions }
            : { name, instructions },
        ),
      );
    },
    onSuccess: () => {
      cache.invalidateQueries({ queryKey: ["skills"] });
      close();
    },
  });
  return (
    <Modal
      title={t(
        skill?.source === "deployment"
          ? "Customize skill"
          : skill
            ? "Edit skill"
            : "New skill",
      )}
      close={close}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate();
        }}
      >
        <label>
          {t("Name")}
          <input
            required
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={!!skill}
            maxLength={64}
            pattern="[a-z0-9_-]+"
            autoComplete="off"
          />
        </label>
        <label>
          {t("Instructions")}
          <textarea
            required
            value={instructions}
            onChange={(e) => setInstructions(e.target.value)}
            rows={10}
            maxLength={49152}
          />
        </label>
        <ErrorNotice error={save.error} />
        <footer>
          <button type="button" onClick={close}>
            {t("Cancel")}
          </button>
          <button className="primary" disabled={save.isPending}>
            {t(save.isPending ? "Saving…" : "Save skill")}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
