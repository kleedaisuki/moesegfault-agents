import { useTranslation } from "react-i18next";
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowUpRight,
  Bot,
  Plus,
  Terminal,
  Settings2,
  Trash2,
} from "lucide-react";
import {
  request,
  apiBaseUrl,
  json,
  type Agent,
  type ModelRegistry,
  type ProviderKey,
  type Skill,
  type ToolServices,
} from "./api";
import { ErrorNotice, Empty, Modal } from "./ui";
import { Conversation } from "./Conversation";
import { ToolServicesEditor } from "./ToolServicesEditor";
const workspaceTools = [
  "bash",
  "workspace_commit",
  "workspace_versions",
  "workspace_restore",
];
const toolLabels = {
  time: "Current time",
  bash: "Bash",
  workspace_commit: "Persist workspace",
  workspace_versions: "List workspace versions",
  workspace_restore: "Restore workspace",
} as const;
export function Agents() {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const agents = useQuery({
    queryKey: ["agents"],
    queryFn: () => request<{ data: Agent[] }>("/v1/agents"),
  });
  const [editing, setEditing] = useState<Agent | "new" | null>(null);
  const [selected, setSelected] = useState<Agent | null>(null);
  const remove = useMutation({
    mutationFn: (id: string) =>
      request(`/v1/agents/${encodeURIComponent(id)}`, json("DELETE")),
    onSuccess: () => cache.invalidateQueries({ queryKey: ["agents"] }),
  });
  if (selected)
    return <Conversation agent={selected} back={() => setSelected(null)} />;
  return (
    <>
      <header className="page-header">
        <div>
          <h1>
            {t("Your agents")}
            <span className="count">{agents.data?.data.length ?? "—"}</span>
          </h1>
        </div>
        <button className="primary" onClick={() => setEditing("new")}>
          <Plus size={17} />
          {t("New agent")}
        </button>
      </header>
      <ErrorNotice error={agents.error ?? remove.error} />
      {agents.isPending ? (
        <Empty title={t("Loading agents…")} />
      ) : agents.data?.data.length ? (
        <div className="agent-grid">
          {agents.data.data.map((agent) => (
            <article className="agent-card" key={agent.id}>
              <div className="card-top">
                <span className="agent-icon">
                  <Bot size={23} />
                </span>
                <span className="chip">{agent.model.provider}</span>
              </div>
              <button className="card-title" onClick={() => setSelected(agent)}>
                {agent.name}
                <ArrowUpRight size={18} />
              </button>
              <p className="model-name">{agent.model.model}</p>
              <div className="card-bottom">
                <span>
                  <Terminal size={14} />
                  {agent.workspace ? t("Workspace enabled") : t("Harness only")}
                </span>
                <div>
                  <button
                    className="icon"
                    aria-label={t("Edit {{name}}", { name: agent.name })}
                    onClick={() => setEditing(agent)}
                  >
                    <Settings2 size={16} />
                  </button>
                  <button
                    className="icon danger"
                    aria-label={t("Delete {{name}}", { name: agent.name })}
                    disabled={remove.isPending}
                    onClick={() => {
                      if (
                        window.confirm(
                          t(
                            "Delete {{name}}? Existing session data is not erased by this action.",
                            { name: agent.name },
                          ),
                        )
                      )
                        remove.mutate(agent.id);
                    }}
                  >
                    <Trash2 size={16} />
                  </button>
                </div>
              </div>
            </article>
          ))}
          <button className="new-card" onClick={() => setEditing("new")}>
            <Plus size={23} />
            <span>{t("New agent")}</span>
          </button>
        </div>
      ) : (
        !agents.error && (
          <Empty title={t("No agents")}>
            <button className="primary" onClick={() => setEditing("new")}>
              <Plus size={16} />
              {t("Create agent")}
            </button>
          </Empty>
        )
      )}
      <details className="api-details">
        <summary>API</summary>
        <pre>{`from openai import OpenAI

client = OpenAI(base_url="${apiBaseUrl()}", api_key="YOUR_API_KEY", max_retries=0)
response = client.responses.create(
    model="AGENT_ID",
    input="Hello",
)
print(response.output_text)`}</pre>
      </details>
      {editing && (
        <AgentEditor
          agent={editing === "new" ? undefined : editing}
          close={() => setEditing(null)}
        />
      )}
    </>
  );
}
function AgentEditor({ agent, close }: { agent?: Agent; close: () => void }) {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const models = useQuery({
    queryKey: ["models"],
    staleTime: 300_000,
    queryFn: () => request<ModelRegistry>("/v1/models"),
  });
  const skills = useQuery({
    queryKey: ["skills"],
    queryFn: () => request<{ data: Skill[] }>("/v1/skills"),
    staleTime: 300_000,
  });
  const [selectedSkills, setSelectedSkills] = useState<string[]>(
    agent?.skills ?? [],
  );
  const [name, setName] = useState(agent?.name ?? "");
  const [model, setModel] = useState(agent?.model.model ?? "");
  const [modelEdited, setModelEdited] = useState(!agent);
  const [provider, setProvider] = useState(agent?.model.provider ?? "");
  const [contextWindow, setContextWindow] = useState(
    agent?.model.context_window ?? 131072,
  );
  const [maxOutputTokens, setMaxOutputTokens] = useState(
    agent?.model.max_output_tokens ?? 8192,
  );
  const [instructions, setInstructions] = useState(agent?.instructions ?? "");
  const [percent, setPercent] = useState(agent?.compaction_percent ?? 80);
  const [workspace, setWorkspace] = useState(agent?.workspace ?? true);
  const [selectedTools, setSelectedTools] = useState<string[]>(
    agent?.tools ??
      (agent
        ? agent.workspace
          ? ["time", "bash"]
          : ["time"]
        : ["time", ...workspaceTools]),
  );
  const [credentialId, setCredentialId] = useState(
    agent?.model.credential_id ?? "",
  );
  const [protocol, setProtocol] = useState<
    NonNullable<Agent["model"]["protocol"]> | ""
  >(agent?.model.protocol ?? "");
  const [protocolExplicit, setProtocolExplicit] = useState(
    agent?.model.protocol !== undefined,
  );
  const [toolServices, setToolServices] = useState<ToolServices>(
    agent?.tool_services ?? {},
  );
  const [servicesEdited, setServicesEdited] = useState(false);
  const credentials = useQuery({
    queryKey: ["provider-keys"],
    enabled: provider === "byok",
    staleTime: 300_000,
    queryFn: () => request<{ data: ProviderKey[] }>("/v1/provider-keys"),
  });
  const platformModels =
    models.data?.available_platform_models ??
    models.data?.available_models ??
    [];
  const platformProviders = [
    ...new Set(platformModels.map((model) => model.provider)),
  ];
  const selectedProvider = provider || platformProviders[0] || "";
  const selectedKey = credentials.data?.data.find(
    (key) => key.id === credentialId,
  );
  const anthropicKey =
    selectedProvider === "byok" && selectedKey?.protocol === "anthropic";
  function modelsFor(nextProvider: string, nextCredentialId: string) {
    const providerModels =
      nextProvider === "byok"
        ? (models.data?.available_models ?? [])
        : platformModels;
    const namespace =
      nextProvider === "byok"
        ? credentials.data?.data.find((key) => key.id === nextCredentialId)
            ?.provider
        : nextProvider;
    return providerModels.filter((model) => model.provider === namespace);
  }
  const selectableModels = modelsFor(selectedProvider, credentialId);
  function changeProvider(nextProvider: string) {
    if (nextProvider === selectedProvider) return;
    setModelEdited(true);
    setProvider(nextProvider);
    const metadata = modelsFor(nextProvider, credentialId).find(
      (option) => option.model === model,
    );
    applyModelTransport(metadata);
    if (!metadata) setModel("");
    else if (nextProvider === "byok") applyModelLimits(metadata);
  }
  function changeCredential(nextCredentialId: string) {
    if (nextCredentialId === credentialId) return;
    setModelEdited(true);
    setCredentialId(nextCredentialId);
    const metadata = modelsFor(selectedProvider, nextCredentialId).find(
      (option) => option.model === model,
    );
    applyModelTransport(metadata);
    if (!metadata) setModel("");
    applyModelLimits(
      metadata ?? { context_window: 131072, max_output_tokens: 8192 },
    );
  }
  function applyModelLimits(metadata: {
    context_window: number;
    max_output_tokens: number;
  }) {
    setContextWindow(metadata.context_window);
    setMaxOutputTokens(metadata.max_output_tokens);
  }
  function applyModelTransport(metadata?: {
    protocol?: Agent["model"]["protocol"];
  }) {
    if (metadata && !protocolExplicit) setProtocol(metadata.protocol ?? "");
  }
  function changeModel(nextModel: string) {
    if (nextModel === model) return;
    setModelEdited(true);
    setModel(nextModel);
    const metadata = selectableModels.find(
      (option) => option.model === nextModel,
    );
    applyModelTransport(metadata);
    if (metadata && selectedProvider === "byok") {
      applyModelLimits(metadata);
    }
  }
  function modelPatch(): Partial<Pick<Agent, "model">> {
    if (!modelEdited) return {};
    if (selectedProvider === "byok" && !selectedKey)
      throw new Error("Select an API key from Access first.");
    if (protocol && (protocol === "anthropic") !== Boolean(anthropicKey))
      throw new Error(
        "The model transport must match the saved key’s authentication family.",
      );
    const metadata =
      selectedProvider === "byok"
        ? {
            model: model.trim(),
            context_window: contextWindow,
            max_output_tokens: maxOutputTokens,
          }
        : selectableModels.find((option) => option.model === model);
    if (!metadata)
      throw new Error(
        "Choose a registered model with available context metadata.",
      );
    if (
      selectedProvider === "byok" &&
      (!metadata.model ||
        !Number.isSafeInteger(contextWindow) ||
        !Number.isSafeInteger(maxOutputTokens) ||
        contextWindow < 2 ||
        maxOutputTokens < 1 ||
        maxOutputTokens >= contextWindow)
    )
      throw new Error("Output tokens must be smaller than the context window.");
    return {
      model: {
        provider: selectedProvider,
        model: metadata.model,
        context_window: metadata.context_window,
        max_output_tokens: metadata.max_output_tokens,
        ...(protocol ? { protocol } : {}),
        ...(selectedProvider === "byok" ? { credential_id: credentialId } : {}),
      },
    };
  }
  const save = useMutation({
    mutationFn: () =>
      request(
        agent ? `/v1/agents/${encodeURIComponent(agent.id)}` : "/v1/agents",
        json(agent ? "PATCH" : "POST", {
          name,
          ...modelPatch(),
          instructions,
          compaction_percent: percent,
          workspace,
          tools: selectedTools.filter(
            (tool) => workspace || !workspaceTools.includes(tool),
          ),
          skills: selectedSkills,
          ...(servicesEdited ? { tool_services: toolServices } : {}),
        }),
      ),
    onSuccess: () => {
      cache.invalidateQueries({ queryKey: ["agents"] });
      close();
    },
  });
  return (
    <Modal title={agent ? t("Edit agent") : t("New agent")} close={close}>
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
            maxLength={100}
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={t("Research companion")}
          />
        </label>
        <div className="form-row">
          <label>
            {t("Provider")}
            <select
              required
              value={selectedProvider}
              onChange={(e) => changeProvider(e.target.value)}
            >
              {!selectedProvider && <option value="">{t("Provider")}</option>}
              {selectedProvider !== "byok" &&
                selectedProvider &&
                !platformProviders.includes(selectedProvider) && (
                  <option value={selectedProvider} disabled>
                    {selectedProvider}
                  </option>
                )}
              {platformProviders.map((value) => (
                <option key={value} value={value}>
                  {t(
                    value === "openrouter"
                      ? "OpenRouter"
                      : value === "deepseek"
                        ? "DeepSeek"
                        : value,
                  )}
                </option>
              ))}
              <option value="byok">{t("Your API key")}</option>
            </select>
          </label>
          <label>
            {t("Model")}
            <input
              required
              list="models"
              value={model}
              onChange={(e) => changeModel(e.target.value)}
              disabled={
                !selectedProvider ||
                (selectedProvider === "byok" && !selectedKey)
              }
              placeholder={t(
                selectedProvider === "byok" && !selectedKey
                  ? "Select an API key first."
                  : "Select or enter model ID",
              )}
            />
            <datalist id="models">
              {selectableModels.map((option) => (
                <option
                  key={`${option.provider}/${option.model}`}
                  value={option.model}
                />
              ))}
            </datalist>
          </label>
        </div>
        {selectedProvider !== "byok" && <ErrorNotice error={models.error} />}
        {selectedProvider === "byok" && (
          <label>
            {t("Saved API key")}
            <select
              required={modelEdited}
              value={credentialId}
              onChange={(e) => changeCredential(e.target.value)}
            >
              <option value="">{t("Select a key")}</option>
              {credentials.data?.data.map((key) => (
                <option value={key.id} key={key.id}>
                  {key.name ??
                    key.provider ??
                    (key.protocol === "anthropic"
                      ? "Anthropic"
                      : t("OpenAI-compatible API"))}
                </option>
              ))}
            </select>
            <ErrorNotice error={credentials.error} />
          </label>
        )}
        <details className="model-limits">
          <summary>{t("Model transport")}</summary>
          <label>
            {t("API transport")}
            <select
              value={protocol}
              onChange={(event) => {
                setModelEdited(true);
                setProtocolExplicit(true);
                setProtocol(
                  event.target.value as
                    NonNullable<Agent["model"]["protocol"]> | "",
                );
              }}
            >
              <option value="">{t("Provider default")}</option>
              <option value="openai" disabled={anthropicKey}>
                {t("Chat Completions")}
              </option>
              <option value="responses" disabled={anthropicKey}>
                Responses
              </option>
              <option value="anthropic" disabled={!anthropicKey}>
                Anthropic Messages
              </option>
            </select>
          </label>
        </details>
        {selectedProvider === "byok" && (
          <details className="model-limits">
            <summary>{t("Model limits")}</summary>
            <div className="form-row">
              <label>
                {t("Context window")}
                <input
                  type="number"
                  min={2}
                  max={4294967295}
                  step={1}
                  required
                  value={contextWindow}
                  onChange={(e) => {
                    setModelEdited(true);
                    setContextWindow(Number(e.target.value));
                  }}
                />
              </label>
              <label>
                {t("Maximum output tokens")}
                <input
                  type="number"
                  min={1}
                  max={contextWindow - 1}
                  step={1}
                  required
                  value={maxOutputTokens}
                  onChange={(e) => {
                    setModelEdited(true);
                    setMaxOutputTokens(Number(e.target.value));
                  }}
                />
              </label>
            </div>
            <p className="hint">
              {t(
                "Use your API’s model limits. Context compaction is calculated from this window.",
              )}
            </p>
          </details>
        )}
        <label>
          {t("Instructions")}
          <textarea
            rows={5}
            value={instructions}
            onChange={(e) => setInstructions(e.target.value)}
            placeholder={t("What should this agent know?")}
          />
        </label>
        <p className="hint">
          {t("Pinned when a session starts. Changes apply to new sessions.")}
        </p>
        <label className="range-label">
          {t("Compact context")}
          <strong>{percent}%</strong>
          <input
            aria-label={t("Compact context percentage")}
            type="range"
            min={1}
            max={99}
            step={1}
            value={percent}
            onChange={(e) => setPercent(Number(e.target.value))}
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={workspace}
            onChange={(e) => {
              setWorkspace(e.target.checked);
              if (
                e.target.checked &&
                !selectedTools.some((tool) => workspaceTools.includes(tool))
              )
                setSelectedTools((previous) => [
                  ...previous,
                  ...workspaceTools,
                ]);
            }}
          />{" "}
          {t("Persistent workspace")}
        </label>
        <details className="model-limits">
          <summary>{t("Tools")}</summary>
          {Object.entries(toolLabels)
            .filter(([tool]) => workspace || tool === "time")
            .map(([tool, label]) => (
              <label className="check" key={tool}>
                <input
                  type="checkbox"
                  checked={selectedTools.includes(tool)}
                  onChange={(event) =>
                    setSelectedTools((previous) =>
                      event.target.checked
                        ? [...previous, tool]
                        : previous.filter((name) => name !== tool),
                    )
                  }
                />
                {t(label)}
              </label>
            ))}
        </details>
        <ToolServicesEditor
          value={toolServices}
          onChange={(next) => {
            setToolServices(next);
            setServicesEdited(true);
          }}
        />
        <fieldset className="skill-selection">
          <legend>{t("Skills")}</legend>
          <ErrorNotice error={skills.error} />
          {skills.data?.data.length
            ? skills.data.data.map((skill) => (
                <label className="check" key={skill.name}>
                  <input
                    type="checkbox"
                    checked={selectedSkills.includes(skill.name)}
                    onChange={(e) =>
                      setSelectedSkills((previous) =>
                        e.target.checked
                          ? [...previous, skill.name]
                          : previous.filter((name) => name !== skill.name),
                      )
                    }
                  />
                  {skill.name}
                </label>
              ))
            : !skills.error && <p className="hint">{t("No skills")}</p>}
        </fieldset>
        <ErrorNotice error={save.error} />
        <footer>
          <button type="button" onClick={close}>
            {t("Cancel")}
          </button>
          <button
            className="primary"
            disabled={
              save.isPending ||
              (modelEdited &&
                (selectedProvider === "byok"
                  ? credentials.isPending
                  : models.isPending))
            }
          >
            {save.isPending ? t("Saving…") : t("Save agent")}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
