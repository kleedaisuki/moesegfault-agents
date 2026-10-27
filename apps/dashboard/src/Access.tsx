import { useTranslation } from "react-i18next";
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Trash2 } from "lucide-react";
import {
  request,
  json,
  formattedDate,
  type ApiKey,
  type ProviderKey,
} from "./api";
import { ErrorNotice } from "./ui";
export function Access() {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const keys = useQuery({
    queryKey: ["keys"],
    queryFn: () => request<{ data: ApiKey[] }>("/v1/api-keys"),
  });
  const providers = useQuery({
    queryKey: ["provider-keys"],
    queryFn: () => request<{ data: ProviderKey[] }>("/v1/provider-keys"),
  });
  const [name, setName] = useState("");
  const [secret, setSecret] = useState("");
  const [protocol, setProtocol] = useState<"openai" | "anthropic">("openai");
  const [baseUrl, setBaseUrl] = useState("");
  const [connectionName, setConnectionName] = useState("");
  const [providerKey, setProviderKey] = useState("");
  const [consent, setConsent] = useState(false);
  const [errorCode, setErrorCode] = useState("");
  const [requestId, setRequestId] = useState("");
  const create = useMutation({
    gcTime: 0,
    mutationFn: () =>
      request<{ key: string }>("/v1/api-keys", json("POST", { name })),
    onSuccess: (data) => {
      setSecret(data.key);
      setName("");
      cache.invalidateQueries({ queryKey: ["keys"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (id: string) =>
      request(`/v1/api-keys/${encodeURIComponent(id)}`, json("DELETE")),
    onSuccess: () => cache.invalidateQueries({ queryKey: ["keys"] }),
  });
  const saveProvider = useMutation({
    gcTime: 0,
    mutationFn: () =>
      request(
        "/v1/provider-keys",
        json("POST", {
          protocol,
          base_url: baseUrl,
          api_key: providerKey,
          ...(connectionName.trim() ? { name: connectionName.trim() } : {}),
        }),
      ),
    onSuccess: () => {
      setProviderKey("");
      setBaseUrl("");
      setConnectionName("");
      cache.invalidateQueries({ queryKey: ["provider-keys"] });
    },
  });
  const removeProvider = useMutation({
    mutationFn: (id: string) =>
      request(`/v1/provider-keys/${encodeURIComponent(id)}`, json("DELETE")),
    onSuccess: () => cache.invalidateQueries({ queryKey: ["provider-keys"] }),
  });
  const diagnosticPayload = {
    consent: true,
    client: "adash",
    version: "0.3.0",
    platform: "browser",
    ...(errorCode ? { error_code: errorCode } : {}),
    ...(requestId ? { request_id: requestId } : {}),
  };
  const diagnostics = useMutation({
    mutationFn: () =>
      request("/v1/diagnostics", json("POST", diagnosticPayload)),
    onSuccess: () => {
      setConsent(false);
      setErrorCode("");
      setRequestId("");
    },
  });
  return (
    <>
      <header className="page-header">
        <div>
          <h1>{t("Access")}</h1>
        </div>
      </header>
      <section className="panel">
        <h2>{t("API keys")}</h2>
        <form
          className="inline-form"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          <input
            aria-label={t("New API key name")}
            placeholder={t("Key name")}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            maxLength={100}
          />
          <button disabled={create.isPending}>{t("Create key")}</button>
        </form>
        <ErrorNotice error={keys.error ?? create.error ?? revoke.error} />
        {secret && (
          <div className="notice">
            <strong>
              {t("Copy this key now. It will not be shown again.")}
            </strong>
            <input
              aria-label={t("New API key")}
              readOnly
              value={secret}
              onFocus={(e) => e.target.select()}
            />
            <button
              onClick={() => {
                setSecret("");
                create.reset();
              }}
            >
              {t("I’ve saved it")}
            </button>
          </div>
        )}
        {keys.isPending ? (
          <p>{t("Loading keys…")}</p>
        ) : keys.data?.data.length ? (
          keys.data.data.map((key) => (
            <div className="list-row" key={key.id}>
              <div>
                <strong>{key.name}</strong>
                {key.created_at && (
                  <small className="key-date">
                    {t("Created {{date}}", {
                      date: formattedDate(key.created_at),
                    })}
                  </small>
                )}
                <code>{key.prefix}…</code>
              </div>
              <button
                className="icon danger"
                aria-label={t("Revoke {{name}}", { name: key.name })}
                onClick={() => {
                  if (
                    window.confirm(
                      t(
                        "Revoke {{name}}? Applications using it will lose access.",
                        { name: key.name },
                      ),
                    )
                  )
                    revoke.mutate(key.id);
                }}
              >
                <Trash2 size={16} />
              </button>
            </div>
          ))
        ) : (
          !keys.error && <p className="hint">{t("No API keys yet.")}</p>
        )}
      </section>
      <section className="panel">
        <h2>{t("Your providers")}</h2>
        <p className="hint">
          {t("Keys are encrypted server-side, never saved in this browser.")}
        </p>
        <form
          className="provider-form"
          onSubmit={(e) => {
            e.preventDefault();
            saveProvider.mutate();
          }}
        >
          <div className="form-row">
            <label>
              {t("API type")}
              <select
                aria-label={t("API type")}
                value={protocol}
                onChange={(e) =>
                  setProtocol(e.target.value as "openai" | "anthropic")
                }
              >
                <option value="openai">{t("OpenAI-compatible API")}</option>
                <option value="anthropic">Anthropic</option>
              </select>
            </label>
            <label>
              {t("Connection name (optional)")}
              <input
                value={connectionName}
                onChange={(e) => setConnectionName(e.target.value)}
                maxLength={100}
                placeholder={t("Personal endpoint")}
              />
            </label>
          </div>
          <label>
            {t("Base URL")}
            <input
              type="url"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              required
              placeholder={
                protocol === "anthropic"
                  ? "https://api.anthropic.com/v1"
                  : "https://api.example.com/v1"
              }
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label>
            {t("Provider API key")}
            <input
              type="password"
              autoComplete="off"
              aria-label={t("Provider API key")}
              value={providerKey}
              onChange={(e) => setProviderKey(e.target.value)}
              required
              placeholder={t("API key")}
            />
          </label>
          <button className="provider-save" disabled={saveProvider.isPending}>
            {t("Save key")}
          </button>
        </form>
        <ErrorNotice
          error={providers.error ?? saveProvider.error ?? removeProvider.error}
        />
        {providers.data?.data.map((p) => (
          <div className="list-row provider-row" key={p.id}>
            <div>
              <strong>
                {p.name ??
                  p.provider ??
                  (p.protocol === "anthropic"
                    ? "Anthropic"
                    : t("OpenAI-compatible API"))}
              </strong>
              {p.base_url && <code>{p.base_url}</code>}
            </div>
            <button
              onClick={() => {
                if (
                  window.confirm(
                    t("Remove {{provider}} key?", {
                      provider: p.name ?? p.provider ?? p.protocol,
                    }),
                  )
                )
                  removeProvider.mutate(p.id);
              }}
            >
              {t("Remove")}
            </button>
          </div>
        ))}
      </section>
      <section className="panel">
        <h2>{t("Diagnostics")}</h2>
        <p className="hint">
          {t(
            "Nothing is sent automatically. Review the exact payload before sharing; never include secrets or conversation content.",
          )}
        </p>
        <div className="form-row">
          <label>
            {t("Error code")}
            <input
              value={errorCode}
              onChange={(e) => {
                setErrorCode(e.target.value.replace(/[^a-zA-Z0-9_.:-]/g, ""));
                setConsent(false);
              }}
              maxLength={64}
              placeholder={t("Optional")}
            />
          </label>
          <label>
            {t("Request ID")}
            <input
              value={requestId}
              onChange={(e) => {
                setRequestId(e.target.value.replace(/[^a-zA-Z0-9_.:-]/g, ""));
                setConsent(false);
              }}
              maxLength={128}
              placeholder={t("Optional")}
            />
          </label>
        </div>
        <pre>{JSON.stringify(diagnosticPayload, null, 2)}</pre>
        <label className="check">
          <input
            type="checkbox"
            checked={consent}
            onChange={(e) => setConsent(e.target.checked)}
          />{" "}
          {t("I authorize sending this diagnostic payload.")}
        </label>
        <button
          disabled={!consent || diagnostics.isPending}
          onClick={() => diagnostics.mutate()}
        >
          {t("Send diagnostics")}
        </button>
        {diagnostics.isSuccess && (
          <p role="status">{t("Diagnostic received.")}</p>
        )}
        <ErrorNotice error={diagnostics.error} />
      </section>
    </>
  );
}
