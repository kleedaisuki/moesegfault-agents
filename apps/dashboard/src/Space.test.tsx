import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Space } from "./Space";
import i18n from "./i18n";
import { setCsrfToken } from "./api";
import {
  formattedBytes,
  type SpaceFile,
  type WorkspaceVersion,
} from "./space-api";

const space = {
  id: "space-1",
  object: "space",
  name: "My space",
  created_at: 1792764000,
  updated_at: 1792764000,
  usage: { bytes: 32, files: 1, limit_bytes: 1048576 },
};
const file: SpaceFile = {
  id: "file one",
  object: "file",
  filename: "notes.md",
  bytes: 32,
  created_at: 1792764000,
  purpose: "user_data",
  status: "processed",
  sha256: "hash",
};
const version: WorkspaceVersion = {
  id: "version one",
  object: "workspace.version",
  name: "Before refactor",
  session_id: "session-1",
  created_at_ms: 1792764000000,
  generation: 2,
  sha256: "hash",
  format_version: 1,
  archive_bytes: 512,
  logical_bytes: 1024,
  entries: 3,
};
let calls: { path: string; init: RequestInit }[];

function mount(
  handler?: (path: string, init: RequestInit) => unknown | Response,
) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      calls.push({ path, init });
      const supplied = handler?.(path, init);
      if (supplied instanceof Response) return supplied;
      const value =
        supplied ??
        (path === "/v1/space"
          ? space
          : path.startsWith("/v1/space/files?")
            ? {
                object: "list",
                data: [file],
                has_more: false,
                last_id: file.id,
              }
            : path.startsWith("/v1/space/workspace-versions?")
              ? {
                  object: "list",
                  data: [version],
                  has_more: false,
                  last_id: version.id,
                }
              : undefined);
      return new Response(
        JSON.stringify(value ?? { error: { message: "Unexpected route" } }),
        { status: value ? 200 : 404 },
      );
    }),
  );
  render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: {
            queries: { retry: false, staleTime: 60000 },
            mutations: { retry: false },
          },
        })
      }
    >
      <Space />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  calls = [];
  i18n.changeLanguage("en");
  setCsrfToken("test-csrf");
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  setCsrfToken(undefined);
});

describe("space user journeys", () => {
  it("loads only file metadata and streams downloads through native links", async () => {
    mount();
    expect(
      await screen.findByRole("heading", { name: "My space" }),
    ).toBeInTheDocument();
    expect(await screen.findByText("notes.md")).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: "Download notes.md" }),
    ).toHaveAttribute("href", "/v1/space/files/file%20one/content");
    expect(
      screen.getByRole("link", { name: "Download notes.md" }),
    ).toHaveAttribute("download");
    expect(calls.map((call) => call.path).sort()).toEqual([
      "/v1/space",
      "/v1/space/files?limit=20",
    ]);
    expect(screen.queryByText("Before refactor")).not.toBeInTheDocument();
  });
  it("uploads FormData with CSRF without forcing JSON or base64 encoding", async () => {
    let uploaded = false;
    mount((path, init) => {
      if (path === "/v1/space/files" && init.method === "POST") {
        const headers = new Headers(init.headers);
        expect(headers.get("x-csrf-token")).toBe("test-csrf");
        expect(headers.has("content-type")).toBe(false);
        expect(init.credentials).toBe("same-origin");
        expect(init.body).toBeInstanceOf(FormData);
        const body = init.body as FormData;
        const selected = body.get("file") as File;
        expect(selected.name).toBe("report.txt");
        expect(selected.size).toBe(7);
        expect(body.get("purpose")).toBe("user_data");
        uploaded = true;
        return { ...file, id: "new-file", filename: "report.txt" };
      }
      if (path.startsWith("/v1/space/files?") && uploaded)
        return {
          data: [file, { ...file, id: "new-file", filename: "report.txt" }],
          has_more: false,
        };
    });
    await screen.findByText("notes.md");
    await userEvent.upload(
      screen.getByLabelText("Choose files"),
      new File(["payload"], "report.txt", { type: "text/plain" }),
    );
    expect(await screen.findByText("report.txt")).toBeInTheDocument();
    expect(calls.filter((call) => call.init.method === "POST")).toHaveLength(1);
  });
  it("renames space and files without changing their immutable identity", async () => {
    let name = space.name;
    let filename = file.filename;
    mount((path, init) => {
      if (path === "/v1/space" && init.method === "PATCH") {
        name = JSON.parse(init.body as string).name;
        return { ...space, name };
      }
      if (path === "/v1/space") return { ...space, name };
      if (path === "/v1/space/files/file%20one" && init.method === "PATCH") {
        filename = JSON.parse(init.body as string).filename;
        return { ...file, filename };
      }
      if (path.startsWith("/v1/space/files?"))
        return { data: [{ ...file, filename }], has_more: false };
    });
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Rename space" }),
    );
    let dialog = within(screen.getByRole("dialog"));
    await user.clear(dialog.getByLabelText("Name"));
    await user.type(dialog.getByLabelText("Name"), "Research");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    expect(
      await screen.findByRole("heading", { name: "Research" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Rename notes.md" }));
    dialog = within(screen.getByRole("dialog"));
    await user.clear(dialog.getByLabelText("Name"));
    await user.type(dialog.getByLabelText("Name"), "renamed.md");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("renamed.md")).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: "Download renamed.md" }),
    ).toHaveAttribute("href", "/v1/space/files/file%20one/content");
  });
  it("requires confirmation for file deletion and refreshes usage", async () => {
    let deleted = false;
    mount((path, init) => {
      if (path === "/v1/space/files/file%20one" && init.method === "DELETE") {
        deleted = true;
        return { id: file.id, deleted: true };
      }
      if (path.startsWith("/v1/space/files?") && deleted)
        return { data: [], has_more: false };
    });
    const confirm = vi
      .spyOn(window, "confirm")
      .mockReturnValueOnce(false)
      .mockReturnValueOnce(true);
    const user = userEvent.setup();
    const button = await screen.findByRole("button", {
      name: "Delete notes.md",
    });
    await user.click(button);
    expect(deleted).toBe(false);
    await user.click(button);
    expect(await screen.findByText("No files")).toBeInTheDocument();
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(
      calls.filter((call) => call.path === "/v1/space").length,
    ).toBeGreaterThan(1);
  });
  it("lazily loads read-only versions and supports keyboard tab navigation", async () => {
    mount();
    await screen.findByText("notes.md");
    const filesTab = screen.getByRole("tab", { name: "Files" });
    filesTab.focus();
    await userEvent.keyboard("{ArrowRight}");
    const versionsTab = screen.getByRole("tab", { name: "Workspace versions" });
    expect(versionsTab).toHaveFocus();
    expect(versionsTab).toHaveAttribute("aria-selected", "true");
    expect(await screen.findByText("Before refactor")).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: "Download Before refactor" }),
    ).toHaveAttribute(
      "href",
      "/v1/space/workspace-versions/version%20one/content",
    );
    expect(
      screen.queryByRole("button", { name: /Delete|Rename Before/ }),
    ).not.toBeInTheDocument();
    expect(
      calls.filter((call) => call.path.includes("workspace-versions")),
    ).toHaveLength(1);
  });
  it("loads the next page only on request and retains already visible files", async () => {
    mount((path) => {
      if (path === "/v1/space/files?limit=20")
        return { data: [file], has_more: true, last_id: file.id };
      if (path === "/v1/space/files?limit=20&after=file+one")
        return {
          data: [{ ...file, id: "older", filename: "older.txt" }],
          has_more: false,
          last_id: "older",
        };
    });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Load more" }));
    expect(await screen.findByText("older.txt")).toBeInTheDocument();
    expect(screen.getByText("notes.md")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Load more" }),
    ).not.toBeInTheDocument();
  });
  it("surfaces upload failure without pretending the file was stored", async () => {
    mount((path, init) =>
      path === "/v1/space/files" && init.method === "POST"
        ? new Response(
            JSON.stringify({ error: { message: "Space limit reached" } }),
            { status: 429 },
          )
        : undefined,
    );
    await userEvent.upload(
      screen.getByLabelText("Choose files"),
      new File(["payload"], "not-stored.txt"),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Space limit reached",
    );
    expect(screen.queryByText("not-stored.txt")).not.toBeInTheDocument();
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Upload files" }),
      ).toBeEnabled(),
    );
  });
});

it("formats transfer sizes without copying content", () => {
  expect(formattedBytes(0)).toBe("0 B");
  expect(formattedBytes(1536)).toBe("1.5 KiB");
  expect(formattedBytes(1048576)).toBe("1 MiB");
  expect(formattedBytes(-1)).toBe("—");
});
