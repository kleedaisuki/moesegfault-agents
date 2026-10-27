import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import {
  request,
  type CodeService,
  type ImageService,
  type ProviderKey,
  type SearchService,
  type ToolServices,
} from "./api";
import { ErrorNotice } from "./ui";

export function ToolServicesEditor({
  value,
  onChange,
}: {
  value: ToolServices;
  onChange: (value: ToolServices) => void;
}) {
  const { t } = useTranslation();
  const [opened, setOpened] = useState(false);
  const [assetHosts, setAssetHosts] = useState(
    (value.image_generation?.asset_hosts ?? []).join("\n"),
  );
  const credentials = useQuery({
    queryKey: ["provider-keys"],
    enabled: opened,
    staleTime: 300_000,
    queryFn: () => request<{ data: ProviderKey[] }>("/v1/provider-keys"),
  });
  const keys = (credentials.data?.data ?? []).filter(
    (key) => key.protocol !== "anthropic",
  );
  const code = value.code_interpreter;
  const image = value.image_generation;
  const search = value.web_search;
  function updateCode(next?: CodeService) {
    const { code_interpreter: _, ...rest } = value;
    onChange(next ? { ...rest, code_interpreter: next } : rest);
  }
  function updateImage(next?: ImageService) {
    const { image_generation: _, ...rest } = value;
    onChange(next ? { ...rest, image_generation: next } : rest);
  }
  function updateSearch(next?: SearchService) {
    const { web_search: _, ...rest } = value;
    onChange(next ? { ...rest, web_search: next } : rest);
  }
  function keyOptions(id?: string) {
    return (
      <>
        <option value="">{t("Select a key")}</option>
        {id && !keys.some((key) => key.id === id) && (
          <option value={id} disabled>
            {t("Unavailable saved key")}
          </option>
        )}
        {keys.map((key) => (
          <option key={key.id} value={key.id}>
            {key.name ?? key.provider ?? key.base_url ?? key.id}
          </option>
        ))}
      </>
    );
  }
  return (
    <details
      className="model-limits"
      onToggle={(event) => {
        setOpened(event.currentTarget.open);
      }}
    >
      <summary>{t("Tool services")}</summary>
      <fieldset className="skill-selection">
        <legend>{t("Code Interpreter")}</legend>
        <label>
          {t("Code service")}
          <select
            value={code ? (code.provider ?? "platform") : ""}
            onChange={(event) =>
              updateCode(
                event.target.value
                  ? { provider: event.target.value as CodeService["provider"] }
                  : undefined,
              )
            }
          >
            <option value="">{t("Platform default")}</option>
            <option value="platform">{t("Platform")}</option>
            <option value="workspace_daemon">
              {t("Compatible executor · your key")}
            </option>
            {code?.provider &&
              !["platform", "workspace_daemon"].includes(code.provider) && (
                <option value={code.provider} disabled>
                  {t("Unavailable saved service")}
                </option>
              )}
          </select>
        </label>
        {code?.provider === "workspace_daemon" && (
          <label>
            {t("Executor API key")}
            <select
              required
              value={code.credential_id ?? ""}
              onChange={(event) =>
                updateCode({ ...code, credential_id: event.target.value })
              }
            >
              {keyOptions(code.credential_id)}
            </select>
          </label>
        )}
      </fieldset>
      <fieldset className="skill-selection">
        <legend>{t("Image generation")}</legend>
        <label>
          {t("Image service")}
          <select
            value={image ? (image.provider ?? "platform") : ""}
            onChange={(event) => {
              setAssetHosts("");
              updateImage(
                event.target.value
                  ? { provider: event.target.value as ImageService["provider"] }
                  : undefined,
              );
            }}
          >
            <option value="">{t("Platform default")}</option>
            <option value="platform">{t("Platform")}</option>
            <option value="openai_images">
              {t("OpenAI Images · your key")}
            </option>
            <option value="openrouter_images">
              {t("OpenRouter Images · your key")}
            </option>
          </select>
        </label>
        {image && image.provider && image.provider !== "platform" && (
          <label>
            {t("Image API key")}
            <select
              required
              value={image.credential_id ?? ""}
              onChange={(event) =>
                updateImage({ ...image, credential_id: event.target.value })
              }
            >
              {keyOptions(image.credential_id)}
            </select>
          </label>
        )}
        {image && (
          <label>
            {t("Image model (optional)")}
            <input
              maxLength={256}
              value={image.model ?? ""}
              placeholder={t("Service default")}
              onChange={(event) => {
                const { model: _, ...rest } = image;
                updateImage(
                  event.target.value
                    ? { ...rest, model: event.target.value }
                    : rest,
                );
              }}
            />
          </label>
        )}
        {image && image.provider && image.provider !== "platform" && (
          <details>
            <summary>{t("Image asset hosts")}</summary>
            <label>
              {t("Allowed asset hosts")}
              <textarea
                rows={2}
                value={assetHosts}
                onChange={(event) => {
                  setAssetHosts(event.target.value);
                  const { asset_hosts: _, ...rest } = image;
                  const hosts = event.target.value.split(/\s+/).filter(Boolean);
                  updateImage(
                    hosts.length ? { ...rest, asset_hosts: hosts } : rest,
                  );
                }}
                placeholder="cdn.example.com"
              />
            </label>
            <p className="hint">
              {t(
                "Only needed for URL image results. One hostname per line, no URLs.",
              )}
            </p>
          </details>
        )}
      </fieldset>
      <fieldset className="skill-selection">
        <legend>{t("Web search")}</legend>
        <label>
          {t("Search service")}
          <select
            value={search ? (search.provider ?? "openai_responses") : ""}
            onChange={(event) => {
              const {
                credential_id: _credential,
                model: _model,
                provider: _provider,
                ...limits
              } = search ?? {};
              updateSearch(
                event.target.value
                  ? {
                      ...limits,
                      provider: event.target.value as SearchService["provider"],
                    }
                  : undefined,
              );
            }}
          >
            <option value="">{t("Platform default")}</option>
            <option value="platform_deepseek">
              {t("DeepSeek Search · platform")}
            </option>
            <option value="deepseek_search">
              {t("DeepSeek Search · your key")}
            </option>
            <option value="openai_responses">
              {t("Responses search · your key")}
            </option>
          </select>
        </label>
        {search && search.provider !== "platform_deepseek" && (
          <label>
            {t("Search API key")}
            <select
              required
              value={search.credential_id ?? ""}
              onChange={(event) =>
                updateSearch({ ...search, credential_id: event.target.value })
              }
            >
              {keyOptions(search.credential_id)}
            </select>
          </label>
        )}
        {search &&
          (search.provider ?? "openai_responses") === "openai_responses" && (
            <>
              <label>
                {t("Search model")}
                <input
                  required
                  maxLength={256}
                  value={search.model ?? ""}
                  onChange={(event) =>
                    updateSearch({ ...search, model: event.target.value })
                  }
                />
              </label>
              <details>
                <summary>{t("Search limits")}</summary>
                <div className="form-row">
                  <label>
                    {t("Search output tokens")}
                    <input
                      required
                      type="number"
                      min={64}
                      max={8192}
                      step={1}
                      value={search.max_output_tokens ?? 1024}
                      onChange={(event) =>
                        updateSearch({
                          ...search,
                          max_output_tokens: Number(event.target.value),
                        })
                      }
                    />
                  </label>
                  <label>
                    {t("Search tool calls")}
                    <input
                      required
                      type="number"
                      min={1}
                      max={8}
                      step={1}
                      value={search.max_tool_calls ?? 4}
                      onChange={(event) =>
                        updateSearch({
                          ...search,
                          max_tool_calls: Number(event.target.value),
                        })
                      }
                    />
                  </label>
                </div>
              </details>
            </>
          )}
      </fieldset>
      <ErrorNotice error={credentials.error} />
    </details>
  );
}
