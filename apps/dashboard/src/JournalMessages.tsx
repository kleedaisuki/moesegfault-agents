import { memo } from "react";
import { useTranslation } from "react-i18next";
import { MessageContent } from "./MessageContent";
import type { JournalEntry } from "./session-data";

const JournalMessage = memo(function JournalMessage({
  message,
  label,
}: Pick<JournalEntry, "message"> & { label: string }) {
  return (
    <article className={`message ${message.role}`}>
      <span>{label}</span>
      <MessageContent content={message.content} />
    </article>
  );
});

export const JournalMessages = memo(function JournalMessages({
  entries,
}: {
  entries: readonly JournalEntry[];
}) {
  const { t } = useTranslation();
  return entries.map(({ sequence, message }) => (
    <JournalMessage key={sequence} message={message} label={t(message.role)} />
  ));
});
