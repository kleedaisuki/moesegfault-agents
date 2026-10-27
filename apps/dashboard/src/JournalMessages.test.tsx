import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { JournalMessages } from "./JournalMessages";
import type { JournalEntry } from "./session-data";
import i18n from "./i18n";

afterEach(cleanup);

it("keeps exact source ordering through append, prepend and replacement", () => {
  const first: JournalEntry = {
    sequence: 1,
    message: { role: "user", content: "one" },
  };
  const second: JournalEntry = {
    sequence: 2,
    message: { role: "assistant", content: "two" },
  };
  const view = render(<JournalMessages entries={[first]} />);
  view.rerender(<JournalMessages entries={[first, second]} />);
  expect(
    screen.getAllByRole("paragraph").map((element) => element.textContent),
  ).toEqual(["one", "two"]);
  const earlier: JournalEntry = {
    sequence: 0,
    message: { role: "tool", content: "zero" },
  };
  view.rerender(<JournalMessages entries={[earlier, first, second]} />);
  expect(
    screen.getAllByRole("paragraph").map((element) => element.textContent),
  ).toEqual(["zero", "one", "two"]);
  view.rerender(
    <JournalMessages
      entries={[
        earlier,
        first,
        { ...second, message: { ...second.message, content: "updated" } },
      ]}
    />,
  );
  expect(screen.getByText("updated")).toBeInTheDocument();
  expect(screen.queryByText("two")).not.toBeInTheDocument();
});

it("updates localized role labels even when the source entries are unchanged", async () => {
  await i18n.changeLanguage("en");
  const entries: JournalEntry[] = [
    { sequence: 0, message: { role: "tool", content: "unchanged source" } },
  ];
  render(<JournalMessages entries={entries} />);
  expect(screen.getByText("tool")).toBeInTheDocument();
  await act(async () => {
    await i18n.changeLanguage("zh-CN");
  });
  expect(screen.getByText("工具")).toBeInTheDocument();
  expect(screen.getByText("unchanged source")).toBeInTheDocument();
  await act(async () => {
    await i18n.changeLanguage("en");
  });
});
