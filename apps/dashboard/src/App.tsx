import { useTranslation } from "react-i18next";
import "./i18n";
import { Preferences } from "./Preferences";
import { lazy, Suspense, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import {
  ArrowUpRight,
  Bot,
  BookOpen,
  ChevronRight,
  KeyRound,
  Layers3,
  LogOut,
} from "lucide-react";
import { ApiError, request, json, setCsrfToken, type Account } from "./api";
import { ErrorNotice, Empty } from "./ui";
import { Agents } from "./Agents";
const Access = lazy(() =>
  import("./Access").then((module) => ({ default: module.Access })),
);
const UsagePage = lazy(() =>
  import("./UsagePage").then((module) => ({ default: module.UsagePage })),
);
const Skills = lazy(() =>
  import("./Skills").then((module) => ({ default: module.Skills })),
);
type Page = "agents" | "skills" | "access" | "usage";
export function App() {
  const { t } = useTranslation();
  const [page, setPage] = useState<Page>("agents");
  const account = useQuery({
    queryKey: ["account"],
    queryFn: async () => {
      const data = await request<Account>("/v1/account");
      setCsrfToken(data.csrf_token);
      return data;
    },
  });
  const logout = useMutation({
    mutationFn: () => request("/auth/logout", json("POST")),
    onSuccess: () => window.location.assign("/"),
  });
  const unauthorized =
    account.error instanceof ApiError && account.error.status === 401;
  return (
    <div className="shell">
      <a className="skip-link" href="#main-content">
        {t("Skip to content")}
      </a>
      <aside className="sidebar">
        <a href="/" className="brand">
          <span className="brand-icon">
            m<span>·</span>
          </span>
          <span>
            moeSegFault<small>{t("agents")}</small>
          </span>
        </a>
        <nav aria-label={t("Main navigation")}>
          {(
            [
              ["agents", Bot, "Agents"],
              ["skills", BookOpen, "Skills"],
              ["access", KeyRound, "Access"],
              ["usage", Layers3, "Usage & plan"],
            ] as const
          ).map(([id, Icon, label]) => (
            <button
              key={id}
              className={page === id ? "nav-item active" : "nav-item"}
              onClick={() => setPage(id)}
            >
              <Icon size={19} />
              {t(label)}
              {page === id && <ChevronRight size={15} />}
            </button>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <Preferences />
          <div className="account">
            <span className="avatar">m</span>
            <div>
              <strong>{account.data?.name ?? t("moeSegFault account")}</strong>
              <small>{t(account.data?.plan ?? "Account")}</small>
            </div>
            {account.data && (
              <button
                className="icon"
                aria-label={t("Sign out")}
                onClick={() => logout.mutate()}
              >
                <LogOut size={16} />
              </button>
            )}
          </div>
        </div>
      </aside>
      <main id="main-content" tabIndex={-1}>
        {account.isPending ? (
          <Empty title={t("Connecting to your workspace…")} />
        ) : unauthorized ? (
          <div className="welcome">
            <h1>{t("Agents")}</h1>
            <a className="button primary" href="/auth/login?return_to=/">
              {t("Continue with moeSegFault")}
              <ArrowUpRight size={18} />
            </a>
          </div>
        ) : account.error ? (
          <>
            <ErrorNotice error={account.error} />
            <button onClick={() => account.refetch()}>{t("Try again")}</button>
          </>
        ) : (
          <>
            {page === "agents" && <Agents />}
            {page === "skills" && (
              <Suspense fallback={<Empty title={t("Loading…")} />}>
                <Skills />
              </Suspense>
            )}
            {page === "access" && (
              <Suspense fallback={<Empty title={t("Loading…")} />}>
                <Access />
              </Suspense>
            )}
            {page === "usage" && (
              <Suspense fallback={<Empty title={t("Loading…")} />}>
                <UsagePage account={account.data!} />
              </Suspense>
            )}
          </>
        )}
        <ErrorNotice error={logout.error} />
      </main>
    </div>
  );
}
