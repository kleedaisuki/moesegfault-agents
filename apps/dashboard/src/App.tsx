import { useTranslation } from "react-i18next";
import "./i18n";
import { Preferences } from "./Preferences";
import { lazy, Suspense, useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Bot,
  BookOpen,
  ChevronRight,
  KeyRound,
  Layers3,
  LogOut,
} from "lucide-react";
import {
  AUTHENTICATION_REQUIRED_EVENT,
  ApiError,
  request,
  json,
  setCsrfToken,
  type Account,
} from "./api";
import { ErrorNotice, Empty } from "./ui";
import { Brand, PublicEntry } from "./PublicEntry";
const Agents = lazy(() =>
  import("./Agents").then((module) => ({ default: module.Agents })),
);
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
function initialPage(): Page {
  const page = window.location.hash.slice(1);
  return page === "skills" || page === "access" || page === "usage"
    ? page
    : "agents";
}
export function App() {
  const { t } = useTranslation();
  const cache = useQueryClient();
  const [page, setPage] = useState<Page>(initialPage);
  const [expired, setExpired] = useState(false);
  useEffect(() => {
    function expireSession() {
      setExpired(true);
      setCsrfToken(undefined);
      void cache.cancelQueries();
      cache.setQueryData<Account | null>(["account"], null);
      cache.removeQueries({
        predicate: (query) => query.queryKey[0] !== "account",
      });
    }
    window.addEventListener(AUTHENTICATION_REQUIRED_EVENT, expireSession);
    return () =>
      window.removeEventListener(AUTHENTICATION_REQUIRED_EVENT, expireSession);
  }, [cache]);
  const account = useQuery<Account | null>({
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
  useEffect(() => {
    if (!unauthorized) return;
    setCsrfToken(undefined);
    cache.setQueryData<Account | null>(["account"], null);
    void cache.cancelQueries({
      predicate: (query) => query.queryKey[0] !== "account",
    });
    cache.removeQueries({
      predicate: (query) => query.queryKey[0] !== "account",
    });
  }, [cache, unauthorized]);
  if (
    expired ||
    unauthorized ||
    account.isPending ||
    account.error ||
    !account.data
  )
    return (
      <PublicEntry
        pending={account.isPending && !expired}
        expired={expired && !unauthorized}
        error={unauthorized || expired ? undefined : account.error}
        retry={() => void account.refetch()}
        page={page}
      />
    );
  function navigate(nextPage: Page) {
    setPage(nextPage);
    window.history.replaceState(
      null,
      "",
      nextPage === "agents" ? "/" : `/#${nextPage}`,
    );
  }
  return (
    <div className="shell">
      <a className="skip-link" href="#main-content">
        {t("Skip to content")}
      </a>
      <aside className="sidebar">
        <Brand />
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
              onClick={() => navigate(id)}
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
        <>
          {page === "agents" && (
            <Suspense fallback={<Empty title={t("Loading…")} />}>
              <Agents />
            </Suspense>
          )}
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
        <ErrorNotice error={logout.error} />
      </main>
    </div>
  );
}
