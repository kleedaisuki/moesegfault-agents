import { useTranslation } from "react-i18next";
import { useEffect, useRef, type ReactNode } from "react";
import { Layers3, X } from "lucide-react";
import { ApiError, localizedError } from "./api";
export function ErrorNotice({ error }: { error: unknown }) {
  const { t } = useTranslation();
  if (!error) return null;
  return (
    <div role="alert" className="notice error">
      <strong>{localizedError(error)}</strong>
      {error instanceof ApiError && error.status === 401 && (
        <p>
          <a href="/auth/login?return_to=/">{t("Sign in again")}</a>
        </p>
      )}
      {error instanceof ApiError && error.requestId && (
        <small>
          {t("Request")} {error.requestId}
        </small>
      )}
    </div>
  );
}
export function Empty({
  title,
  children,
}: {
  title: string;
  children?: ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <div className="empty">
      <Layers3 size={30} />
      <h3>{title}</h3>
      {children}
    </div>
  );
}
export function Modal({
  title,
  children,
  close,
}: {
  title: string;
  children: ReactNode;
  close: () => void;
}) {
  const { t } = useTranslation();
  const dialog = useRef<HTMLDialogElement>(null);
  const trigger = useRef(
    document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null,
  );
  const closeRef = useRef(close);
  closeRef.current = close;
  useEffect(() => {
    const element = dialog.current!;
    element.showModal();
    return () => {
      element.close();
      queueMicrotask(() => {
        if (!element.isConnected && trigger.current?.isConnected)
          trigger.current.focus();
      });
    };
  }, []);
  return (
    <dialog
      ref={dialog}
      className="scrim"
      aria-label={title}
      onCancel={(event) => {
        event.preventDefault();
        closeRef.current();
      }}
      onClick={(event) => {
        if (event.target === event.currentTarget) close();
      }}
    >
      <section className="modal" onClick={(e) => e.stopPropagation()}>
        <header>
          <h2>{title}</h2>
          <button
            className="icon"
            aria-label={t("Close dialog")}
            onClick={close}
          >
            <X size={20} />
          </button>
        </header>
        {children}
      </section>
    </dialog>
  );
}

export function Failure({ error }: { error: unknown }) {
  const { t } = useTranslation();
  return (
    <main className="failure">
      <ErrorNotice error={error} />
      <button onClick={() => window.location.reload()}>{t("Reload")}</button>
    </main>
  );
}
