import { test, expect } from "@playwright/test";

test("Responses attachments, custom instructions and workspace actions on a narrow screen", async ({
  page,
}) => {
  await page.setViewportSize({ width: 320, height: 900 });
  await page.addInitScript(() => {
    localStorage.setItem("magents.language", "en");
    localStorage.setItem("magents.theme", "dark");
  });
  const agent = {
    id: "agent-responses",
    name: "Research",
    instructions: "Pinned agent directions",
    model: {
      provider: "deepseek",
      model: "flash",
      context_window: 100000,
      max_output_tokens: 8192,
    },
    tools: [
      "time",
      "bash",
      "workspace_commit",
      "workspace_versions",
      "workspace_restore",
    ],
    compaction_percent: 80,
    workspace: true,
    skills: [],
  };
  let completed = false;
  const imageUrl =
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5n8AAAAASUVORK5CYII=";
  const remoteImages: string[] = [];
  page.on("request", (request) => {
    if (request.url().includes("tracker.invalid"))
      remoteImages.push(request.url());
  });
  const writes: { path: string; body: unknown }[] = [];
  await page.route("**/v1/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    const method = request.method();
    if (method !== "GET") {
      expect(request.headers()["x-csrf-token"]).toBe("response-csrf");
      writes.push({ path, body: request.postDataJSON() });
      if (path.includes("/workspace/"))
        expect(request.postDataJSON().operation_id).toMatch(
          /^[A-Za-z0-9_-]{1,128}$/,
        );
    }
    if (path === "/v1/account")
      return route.fulfill({
        json: {
          account_id: "owned",
          plan: "free",
          csrf_token: "response-csrf",
        },
      });
    if (path === "/v1/agents")
      return route.fulfill({ json: { data: [agent] } });
    if (path === "/v1/agents/agent-responses/sessions")
      return route.fulfill({
        json: {
          data: [
            { id: "session-responses", agent_id: agent.id, status: "idle" },
          ],
        },
      });
    if (path === "/v1/sessions/session-responses/view")
      return route.fulfill({
        json: {
          id: "session-responses",
          agent_id: agent.id,
          status: "idle",
          message_count: completed ? 2 : 0,
          latest_response_id: completed ? "resp-current" : "resp-parent",
        },
      });
    if (path === "/v1/sessions/session-responses/messages")
      return route.fulfill({
        json: {
          object: "list",
          data: completed
            ? [
                {
                  sequence: 0,
                  message: {
                    role: "user",
                    content: [
                      { type: "text", text: "Read this brief" },
                      { type: "image_url", image_url: { url: imageUrl } },
                      {
                        type: "image_url",
                        image_url: { url: "https://tracker.invalid/pixel.png" },
                      },
                    ],
                  },
                },
                {
                  sequence: 1,
                  message: {
                    role: "assistant",
                    content: [{ type: "output_text", text: "Brief reviewed." }],
                  },
                },
              ]
            : [],
          start: 0,
          end: completed ? 2 : 0,
          high_water: completed ? 2 : 0,
          before_cursor: "before-0",
          after_cursor: completed ? "after-2" : "after-0",
          has_older: false,
          has_newer: false,
        },
      });
    if (path === "/v1/space/files")
      return route.fulfill({
        json: {
          object: "list",
          data: [
            {
              id: "file-image",
              filename: "extensionless",
              content_type: "image/png",
              bytes: 68,
            },
            {
              id: "file-brief",
              object: "file",
              filename:
                "a-long-project-brief-that-must-wrap-on-small-mobile-screens-without-overflow.md",
              bytes: 4096,
              created_at: 1792764000,
            },
          ],
          has_more: false,
        },
      });
    if (path === "/v1/responses") {
      expect(request.headers()["x-agents-session"]).toBe("session-responses");
      expect(request.postDataJSON()).toEqual({
        model: agent.id,
        input: [
          {
            role: "user",
            content: [
              { type: "input_text", text: "Read this brief" },
              { type: "input_file", file_id: "file-brief" },
              { type: "input_image", file_id: "file-image" },
            ],
          },
        ],
        previous_response_id: "resp-parent",
        instructions: "Answer with a checklist",
        stream: true,
      });
      completed = true;
      const events = [
        {
          type: "response.created",
          response: { id: "resp-current", status: "in_progress" },
        },
        { type: "response.output_text.delta", delta: "Brief reviewed." },
        {
          type: "response.completed",
          response: {
            id: "resp-current",
            status: "completed",
            output: [{ type: "message" }],
          },
        },
      ];
      return route.fulfill({
        contentType: "text/event-stream",
        body: events
          .map(
            (event) =>
              `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`,
          )
          .join(""),
      });
    }
    if (path === "/v1/space/workspace-versions")
      return route.fulfill({
        json: {
          object: "list",
          data: [
            {
              id: "version-stable",
              name: "Stable version",
              created_at_ms: 1792764000000,
              archive_bytes: 500,
            },
          ],
          has_more: false,
        },
      });
    if (
      path === "/v1/sessions/session-responses/workspace/commit" ||
      path === "/v1/sessions/session-responses/workspace/restore"
    )
      return route.fulfill({
        json: { id: "version-saved", object: "workspace.version" },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected route ${method} ${path}` } },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Research", exact: true }).click();
  await page.getByRole("button", { name: /session-/ }).click();
  await page.getByRole("button", { name: "Attach files" }).click();
  let dialog = page.getByRole("dialog");
  await dialog.getByRole("checkbox", { name: /a-long-project-brief/ }).check();
  await dialog.getByRole("checkbox", { name: /extensionless/ }).check();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await dialog.getByRole("button", { name: "Attach selected" }).click();
  await page.locator(".response-instructions summary").click();
  await page
    .getByRole("textbox", { name: "Response instructions" })
    .fill("Answer with a checklist");
  await page
    .getByRole("textbox", { name: "Message or steering instruction" })
    .fill("Read this brief");
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(
    page.getByText("Brief reviewed.", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("img", { name: "Attached image" })).toBeVisible();
  await expect
    .poll(() =>
      page
        .getByRole("img", { name: "Attached image" })
        .evaluate((image: HTMLImageElement) => image.naturalWidth),
    )
    .toBe(1);
  await expect(page.getByText(/tracker.invalid/)).toBeVisible();
  expect(remoteImages).toEqual([]);
  await expect(
    page.getByRole("button", { name: /Remove a-long-project/ }),
  ).not.toBeVisible();
  await page.locator(".workspace-controls summary").click();
  await page
    .getByRole("button", { name: "Persist workspace", exact: true })
    .click();
  dialog = page.getByRole("dialog");
  await dialog.getByLabel("Version name (optional)").fill("Reviewed brief");
  await dialog.getByRole("button", { name: "Persist workspace" }).click();
  await expect(dialog).not.toBeVisible();
  await page
    .getByRole("button", { name: "Restore workspace", exact: true })
    .click();
  dialog = page.getByRole("dialog");
  await expect(
    dialog.getByRole("button", { name: "Restore workspace" }),
  ).toBeDisabled();
  await dialog.getByRole("radio", { name: /Stable version/ }).check();
  await dialog.getByRole("button", { name: "Restore workspace" }).click();
  await expect(dialog).not.toBeVisible();
  expect(writes).toHaveLength(3);
  await expect(
    page.getByRole("button", { name: "Restore workspace", exact: true }),
  ).toBeFocused();
  expect(writes[1]).toEqual({
    path: "/v1/sessions/session-responses/workspace/commit",
    body: { name: "Reviewed brief", operation_id: expect.any(String) },
  });
  expect(writes[2]).toEqual({
    path: "/v1/sessions/session-responses/workspace/restore",
    body: { version_id: "version-stable", operation_id: expect.any(String) },
  });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({
    path: "../../.cache/dashboard-e2e/responses-narrow.png",
    fullPage: true,
  });
});
