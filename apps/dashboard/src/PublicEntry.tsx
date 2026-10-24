import { ArrowUpRight, ArrowRight, LoaderCircle, Terminal } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Preferences } from "./Preferences";
import { ErrorNotice } from "./ui";

export function Brand() {
  const { t } = useTranslation();
  return (
    <a href="/" className="brand" aria-label="moeSegFault agents">
      <span className="brand-icon" aria-hidden="true">
        m<span>·</span>
      </span>
      <span>
        moeSegFault<small>{t("agents")}</small>
      </span>
    </a>
  );
}

export function PublicEntry({
  pending,
  expired,
  error,
  retry,
  page,
}: {
  pending: boolean;
  expired: boolean;
  error?: unknown;
  retry: () => void;
  page: string;
}) {
  const { t } = useTranslation();
  const login = `/auth/login?return_to=/${page === "agents" ? "" : encodeURIComponent(`#${page}`)}`;
  return (
    <div className="public-shell">
      <a className="skip-link" href="#main-content">
        {t("Skip to content")}
      </a>
      <header className="public-header">
        <Brand />
        <Preferences />
      </header>
      <main
        id="main-content"
        tabIndex={-1}
        className={
          pending || error ? "public-main public-connection" : "public-main"
        }
      >
        {pending ? (
          <div className="connection-state" role="status">
            <LoaderCircle
              className="loading-icon"
              size={24}
              aria-hidden="true"
            />
            <p>{t("Checking your session…")}</p>
          </div>
        ) : error ? (
          <div className="connection-state">
            <h1>{t("Let’s reconnect.")}</h1>
            <ErrorNotice error={error} />
            <button onClick={retry}>
              {t("Try again")}
              <ArrowRight size={17} />
            </button>
          </div>
        ) : (
          <>
            <section className="public-intro">
              <span className="public-eyebrow">API / CLI / WORKSPACE</span>
              <h1>
                {t("Your agents.")}
                <br />
                <span>{t("Ready for real work.")}</span>
              </h1>
              <p className="public-description">
                {t(
                  "A persistent workspace, your choice of model, and one API to bring it together.",
                )}
              </p>
              <a className="button primary public-sign-in" href={login}>
                {t("Continue with moeSegFault")}
                <ArrowUpRight size={18} />
              </a>
              {expired && (
                <p className="session-ended" role="status">
                  {t("Your session has ended. Sign in to continue.")}
                </p>
              )}
            </section>
            <div
              className="public-terminal"
              aria-label={t("Command line example")}
            >
              <div className="terminal-bar">
                <Terminal size={16} aria-hidden="true" />
                <span>magents</span>
                <span className="terminal-dots" aria-hidden="true">
                  <i />
                  <i />
                  <i />
                </span>
              </div>
              <pre>
                <code>
                  <span className="terminal-prompt">$</span> magents auth login
                  {"\n\n"}
                  <span className="terminal-prompt">$</span> magents agents
                  publish agent.toml{"\n\n"}
                  <span className="terminal-prompt">$</span> magents run
                  AGENT_ID{` \\`}
                  {"\n"} "{t("Pick up where we left off.")}"
                </code>
              </pre>
              <div className="terminal-bottom">
                <span className="terminal-status" aria-hidden="true" />
                {t("Your context. Your workspace.")}
              </div>
            </div>
          </>
        )}
      </main>
      <footer className="public-footer">
        <span>moeSegFault Agents</span>
        <div>
          <a
            href="https://github.com/kleedaisuki/moesegfault-agents#readme"
            target="_blank"
            rel="noreferrer"
          >
            {t("Documentation")}
            <ArrowUpRight size={14} />
          </a>
          <a
            href="https://github.com/kleedaisuki/moesegfault-agents/releases"
            target="_blank"
            rel="noreferrer"
          >
            {t("Download CLI")}
            <ArrowUpRight size={14} />
          </a>
        </div>
      </footer>
    </div>
  );
}
