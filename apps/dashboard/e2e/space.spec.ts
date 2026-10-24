import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";

for (const fixture of [
  {
    language: "en",
    theme: "light",
    width: 1280,
    label: "desktop",
    space: "Space",
    files: "Files",
    versions: "Workspace versions",
    renameSpace: "Rename space",
    name: "Name",
    save: "Save",
    renameFile: "Rename notes.txt",
    deleteFile: "Delete renamed.txt",
    download: "Download renamed.txt",
    versionDownload: "Download Known good",
  },
  {
    language: "zh-CN",
    theme: "dark",
    width: 390,
    label: "mobile",
    space: "空间",
    files: "文件",
    versions: "工作区版本",
    renameSpace: "重命名空间",
    name: "名称",
    save: "保存",
    renameFile: "重命名 notes.txt",
    deleteFile: "删除 renamed.txt",
    download: "下载 renamed.txt",
    versionDownload: "下载 Known good",
  },
  {
    language: "zh-CN",
    theme: "dark",
    width: 320,
    label: "narrow",
    space: "空间",
    files: "文件",
    versions: "工作区版本",
    renameSpace: "重命名空间",
    name: "名称",
    save: "保存",
    renameFile: "重命名 notes.txt",
    deleteFile: "删除 renamed.txt",
    download: "下载 renamed.txt",
    versionDownload: "下载 Known good",
  },
] as const) {
  test(`space upload, management and immutable version downloads ${fixture.label}`, async ({
    page,
  }) => {
    await page.setViewportSize({ width: fixture.width, height: 844 });
    await page.addInitScript(({ language, theme }) => {
      localStorage.setItem("magents.language", language);
      localStorage.setItem("magents.theme", theme);
    }, fixture);
    let name = "Research space";
    let files: {
      id: string;
      object: string;
      filename: string;
      bytes: number;
      created_at: number;
      purpose: string;
      status: string;
      sha256: string;
    }[] = [];
    const requests: string[] = [];
    await page.context().route("**/v1/**", async (route) => {
      const request = route.request();
      const url = new URL(request.url());
      const path = url.pathname;
      const method = request.method();
      requests.push(`${method} ${path}`);
      if (method !== "GET")
        expect(request.headers()["x-csrf-token"]).toBe("space-csrf");
      if (path === "/v1/account")
        return route.fulfill({
          json: { account_id: "owned", plan: "free", csrf_token: "space-csrf" },
        });
      if (path === "/v1/space") {
        if (method === "PATCH") name = request.postDataJSON().name;
        return route.fulfill({
          json: {
            id: "owned-space",
            object: "space",
            name,
            created_at: 1792764000,
            updated_at: 1792764000,
            usage: {
              bytes: files.reduce((sum, file) => sum + file.bytes, 0),
              files: files.length,
              limit_bytes: 1048576,
            },
          },
        });
      }
      if (path === "/v1/space/files") {
        if (method === "POST") {
          expect(request.headers()["content-type"]).toMatch(
            /^multipart\/form-data; boundary=/,
          );
          expect(request.postData()).toContain('filename="notes.txt"');
          expect(request.postData()).toContain("uploaded bytes");
          files = [
            {
              id: "file-owned",
              object: "file",
              filename: "notes.txt",
              bytes: 14,
              created_at: 1792764000,
              purpose: "user_data",
              status: "processed",
              sha256: "fixture-digest",
            },
          ];
          return route.fulfill({ status: 201, json: files[0] });
        }
        expect(url.searchParams.get("limit")).toBe("20");
        return route.fulfill({
          json: {
            object: "list",
            data: files,
            has_more: false,
            last_id: files.at(-1)?.id,
          },
        });
      }
      if (path === "/v1/space/files/file-owned") {
        if (method === "PATCH") {
          files[0].filename = request.postDataJSON().filename;
          return route.fulfill({ json: files[0] });
        }
        if (method === "DELETE") {
          files = [];
          return route.fulfill({
            json: { id: "file-owned", object: "file", deleted: true },
          });
        }
      }
      if (path === "/v1/space/files/file-owned/content")
        return route.fulfill({
          body: "uploaded bytes",
          headers: {
            "Content-Type": "application/octet-stream",
            "Content-Disposition": 'attachment; filename="renamed.txt"',
          },
        });
      if (path === "/v1/space/workspace-versions")
        return route.fulfill({
          json: {
            object: "list",
            data: [
              {
                id: "version-owned",
                object: "workspace.version",
                name: "Known good",
                session_id: "session-owned",
                created_at_ms: 1792764000000,
                generation: 3,
                archive_bytes: 512,
                logical_bytes: 1024,
                entries: 2,
                sha256: "fixture-version-digest",
                format_version: 1,
              },
            ],
            has_more: false,
            last_id: "version-owned",
          },
        });
      if (path === "/v1/space/workspace-versions/version-owned/content")
        return route.fulfill({
          body: "archive fixture",
          headers: {
            "Content-Type": "application/zstd",
            "Content-Disposition": 'attachment; filename="workspace.tar.zst"',
          },
        });
      return route.fulfill({
        status: 404,
        json: { error: { message: `Unexpected test route ${method} ${path}` } },
      });
    });
    await page.goto("/#space");
    await expect(page.getByRole("heading", { name })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute(
      "data-theme",
      fixture.theme,
    );
    expect(
      requests.some((request) => request.includes("workspace-versions")),
    ).toBe(false);
    await page.getByRole("button", { name: fixture.renameSpace }).click();
    let dialog = page.getByRole("dialog");
    await dialog.getByLabel(fixture.name, { exact: true }).fill("My research");
    await dialog
      .getByRole("button", { name: fixture.save, exact: true })
      .click();
    await expect(
      page.getByRole("heading", { name: "My research" }),
    ).toBeVisible();
    await page.locator('input[type="file"]').setInputFiles({
      name: "notes.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("uploaded bytes"),
    });
    await expect(page.getByText("notes.txt", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: fixture.renameFile }).click();
    dialog = page.getByRole("dialog");
    await dialog.getByLabel(fixture.name, { exact: true }).fill("renamed.txt");
    await dialog
      .getByRole("button", { name: fixture.save, exact: true })
      .click();
    await expect(page.getByText("renamed.txt", { exact: true })).toBeVisible();
    const fileDownload = page.waitForEvent("download");
    await page.getByRole("link", { name: fixture.download }).click();
    const downloadedFile = await fileDownload;
    expect(downloadedFile.suggestedFilename()).toBe("renamed.txt");
    expect(await readFile((await downloadedFile.path())!, "utf8")).toBe(
      "uploaded bytes",
    );
    await page.getByRole("tab", { name: fixture.versions }).click();
    await expect(page.getByText("Known good", { exact: true })).toBeVisible();
    expect(await page.getByRole("tabpanel").getByRole("button").count()).toBe(
      1,
    );
    const versionDownload = page.waitForEvent("download");
    await page.getByRole("link", { name: fixture.versionDownload }).click();
    const downloadedVersion = await versionDownload;
    expect(downloadedVersion.suggestedFilename()).toBe("workspace.tar.zst");
    expect(await readFile((await downloadedVersion.path())!, "utf8")).toBe(
      "archive fixture",
    );
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page.screenshot({
      path: `../../.cache/dashboard-e2e/space-${fixture.label}.png`,
      fullPage: true,
    });
    await page.getByRole("tab", { name: fixture.files, exact: true }).click();
    page.once("dialog", (confirmation) => confirmation.accept());
    await page.getByRole("button", { name: fixture.deleteFile }).click();
    await expect(
      page.getByText("renamed.txt", { exact: true }),
    ).not.toBeVisible();
    expect(
      requests.filter((request) => request.startsWith("POST /v1/space/files")),
    ).toHaveLength(1);
    expect(
      requests.some((request) =>
        request.startsWith("GET /v1/space/workspace-versions"),
      ),
    ).toBe(true);
    expect(
      requests.some((request) =>
        /^(POST|PATCH|DELETE) \/v1\/space\/workspace-versions/.test(request),
      ),
    ).toBe(false);
  });
}
