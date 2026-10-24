import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { RefreshCw, ArrowUpRight } from "lucide-react";
import {
  request,
  dollars,
  microDollars,
  usageTotals,
  type Usage,
  type Account,
} from "./api";
import { ErrorNotice } from "./ui";
export function UsagePage({ account }: { account: Account }) {
  const { t } = useTranslation();
  const usage = useQuery({
    queryKey: ["usage"],
    queryFn: () => request<Usage>("/v1/usage"),
  });
  const totals = usageTotals(usage.data);
  return (
    <>
      {new URLSearchParams(window.location.search).get("billing_refresh") ===
        "failed" && (
        <div className="notice" role="alert">
          <p>
            {t(
              "Your subscription could not be refreshed. Existing access has not been removed.",
            )}
          </p>
          <a href="/billing/return">{t("Retry subscription refresh")}</a>
        </div>
      )}
      <header className="page-header">
        <div>
          <h1>{t("Usage & plan")}</h1>
        </div>
        <button onClick={() => usage.refetch()} disabled={usage.isFetching}>
          <RefreshCw size={16} />
          {t("Refresh")}
        </button>
      </header>
      <ErrorNotice error={usage.error} />
      {account.billing_mode === "test" && (
        <div className="notice" role="status">
          {t("Test credits · No real charges")}
        </div>
      )}
      <div className="metric-grid">
        {[
          ["Total usage", totals?.total?.toString()],
          ["Model API", totals?.model?.toString()],
          ["Compute", totals?.container?.toString()],
          ["Storage", totals?.storage?.toString()],
        ].map(([label, value]) => (
          <section className="metric" key={label}>
            <span>{t(label as string)}</span>
            <strong>{microDollars(value as string | undefined)}</strong>
          </section>
        ))}
      </div>
      {usage.data && (
        <details className="resource-details panel">
          <summary>{t("Resources")}</summary>
          <div className="resource-table">
            <table>
              <thead>
                <tr>
                  <th>{t("Resource")}</th>
                  <th>{t("Recorded usage")}</th>
                </tr>
              </thead>
              <tbody>
                {usage.data.by_resource.map((row) => (
                  <tr key={row.resource}>
                    <td>{t(`resource.${row.resource}`)}</td>
                    <td>{microDollars(row.cost_microusd)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="hint">
            {t("Recorded usage includes pending settlement.")}
          </p>
        </details>
      )}
      <section className="panel plan">
        <div>
          <span className="eyebrow">{t("CURRENT PLAN")}</span>
          <h2>{t(account.plan)}</h2>
          <p>
            {account.limits?.agents ?? "—"}
            {t("agent configurations ·")}{" "}
            {account.limits?.concurrent_sessions ?? "—"}
            {t("concurrent sessions")}
          </p>
          <p>
            {account.billing_mode === "test"
              ? t("Test credit")
              : t("Available credit")}
            :{" "}
            {usage.data
              ? microDollars(usage.data.wallet.available_microusd)
              : dollars(
                  account.credit_microusd === undefined
                    ? undefined
                    : account.credit_microusd / 1_000_000,
                )}
          </p>
          {usage.data && (
            <p>
              {t("Reserved")}:{" "}
              {microDollars(usage.data.wallet.reserved_microusd)}
            </p>
          )}
        </div>
        {account.subscription_url && (
          <a className="button primary" href={account.subscription_url}>
            {t("Manage subscription")}
            <ArrowUpRight size={16} />
          </a>
        )}
      </section>
      <p className="hint">
        {t(
          "Model usage is charged at API-equivalent rates. Compute and storage are metered separately.",
        )}
      </p>
    </>
  );
}
