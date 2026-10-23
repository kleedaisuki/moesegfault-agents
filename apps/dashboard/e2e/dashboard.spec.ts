import { test, expect } from "@playwright/test";
test("keyboard agent creation and mobile layout", async ({ page }) => {
  const agent = {
    id: "agent-test",
    name: "Research companion",
    model: {
      provider: "deepseek",
      model: "deepseek-flash",
      context_window: 1000000,
      max_output_tokens: 8192,
    },
    instructions: "",
    compaction_percent: 80,
    workspace: true,
    skills: [],
  };
  let agents: unknown[] = [];
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/v1/account")
      return route.fulfill({
        json: {
          account_id: "test",
          plan: "free",
          csrf_token: "test-csrf",
          limits: { agents: 3, concurrent_sessions: 1 },
        },
      });
    if (path === "/v1/skills") return route.fulfill({ json: { data: [] } });
    if (path === "/v1/skills") return route.fulfill({ json: { data: [] } });
    if (path === "/v1/models")
      return route.fulfill({
        json: { data: [], available_models: [agent.model] },
      });
    if (path === "/v1/agents" && route.request().method() === "POST") {
      expect(route.request().headers()["x-csrf-token"]).toBe("test-csrf");
      agents = [agent];
      return route.fulfill({ status: 201, json: agent });
    }
    if (path === "/v1/agents") return route.fulfill({ json: { data: agents } });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Unexpected test route" } },
    });
  });
  await page.goto("/");
  await expect(page.getByText("Cloud workspace", { exact: true })).toHaveCount(
    0,
  );
  await expect(page.locator(".api-details")).not.toHaveAttribute("open");
  await page.locator(".api-details summary").click();
  await expect(page.locator(".api-details pre")).toContainText(
    'base_url="http://127.0.0.1:5173/v1"',
  );
  await page.locator(".api-details summary").click();
  await page.getByRole("button", { name: "New agent", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New agent" });
  await expect(dialog).toBeVisible();
  await dialog.getByLabel("Name", { exact: true }).fill("Research companion");
  await dialog
    .getByRole("combobox", { name: "Provider", exact: true })
    .selectOption("deepseek");
  await dialog
    .getByRole("combobox", { name: "Model", exact: true })
    .fill("deepseek-flash");
  await dialog.getByLabel("Compact context percentage").focus();
  await page.keyboard.press("ArrowLeft");
  await expect(dialog.getByText("79%", { exact: true })).toBeVisible();
  await dialog.getByRole("button", { name: "Save agent" }).click();
  await expect(dialog).not.toBeVisible();
  await expect(
    page.getByRole("button", { name: "Research companion", exact: true }),
  ).toBeVisible();
  await page.screenshot({
    path: "../../.cache/dashboard-e2e-desktop.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(
    page.getByRole("button", { name: "Research companion", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "../../.cache/dashboard-e2e-mobile.png",
    fullPage: true,
  });
});

test("dialog contains keyboard focus and restores the trigger", async ({
  page,
}) => {
  await page.route("**/v1/account", (route) =>
    route.fulfill({ json: { account_id: "test", plan: "free" } }),
  );
  await page.route("**/v1/agents", (route) =>
    route.fulfill({ json: { data: [] } }),
  );
  await page.route("**/v1/models", (route) =>
    route.fulfill({ json: { data: [], available_models: [] } }),
  );
  await page.route("**/v1/skills", (route) =>
    route.fulfill({ json: { data: [] } }),
  );
  await page.route("**/v1/skills", (route) =>
    route.fulfill({ json: { data: [] } }),
  );
  await page.goto("/");
  const trigger = page.getByRole("button", { name: "New agent", exact: true });
  await trigger.click();
  const dialog = page.getByRole("dialog");
  for (let i = 0; i < 16; i++) {
    await page.keyboard.press("Tab");
    expect(
      await dialog.evaluate(
        (el) =>
          el.contains(document.activeElement) ||
          document.activeElement === document.body,
      ),
    ).toBe(true);
  }
  await page.keyboard.press("Escape");
  await expect(dialog).not.toBeVisible();
  await expect(trigger).toBeFocused();
});
