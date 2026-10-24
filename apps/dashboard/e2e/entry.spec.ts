import { test, expect } from "@playwright/test";
import en from "../src/locales/en.json" with { type: "json" };
import zh from "../src/locales/zh-CN.json" with { type: "json" };

for (const scenario of [
  { language: "en", theme: "light", width: 1280 },
  { language: "zh-CN", theme: "dark", width: 1280 },
  { language: "zh-CN", theme: "light", width: 320 },
  { language: "en", theme: "dark", width: 360 },
] as const) {
  test(`${scenario.language} ${scenario.theme} ${scenario.width}px public entry is separate from the dashboard`, async ({
    page,
  }) => {
    const strings: Record<string, string> =
      scenario.language === "en" ? en : zh;
    const t = (key: string) => strings[key] ?? key;
    await page.setViewportSize({ width: scenario.width, height: 900 });
    await page.addInitScript(({ language, theme }) => {
      localStorage.setItem("magents.language", language);
      localStorage.setItem("magents.theme", theme);
    }, scenario);
    const requested: string[] = [];
    await page.route("**/v1/**", async (route) => {
      requested.push(new URL(route.request().url()).pathname);
      return route.fulfill({
        status: 401,
        json: { error: { message: "Sign in" } },
      });
    });
    await page.goto("/");
    await expect(
      page.getByRole("link", { name: t("Continue with moeSegFault") }),
    ).toHaveAttribute("href", "/auth/login?return_to=/");
    await expect(page.getByRole("navigation")).toHaveCount(0);
    await expect(
      page.locator(".sidebar,.account,.agent-grid,.api-details"),
    ).toHaveCount(0);
    await expect(page.getByRole("heading", { level: 1 })).toContainText(
      t("Ready for real work."),
    );
    await expect(page.getByLabel(t("Language"))).toBeVisible();
    await expect(page.getByLabel(t("Appearance"))).toBeVisible();
    expect(requested).toEqual(["/v1/account"]);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    const targets = await page
      .locator("a:visible,select:visible")
      .evaluateAll((elements) =>
        elements.map((element) => element.getBoundingClientRect().height),
      );
    expect(targets.filter((height) => height < 43.9)).toEqual([]);
    await page.screenshot({
      path: `../../.cache/dashboard-entry-${scenario.language}-${scenario.theme}-${scenario.width}.png`,
      fullPage: true,
    });
  });
}

test("session verification never flashes management navigation or starts private data requests", async ({
  page,
}) => {
  let release!: () => void;
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  const requests: string[] = [];
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    requests.push(path);
    if (path === "/v1/account") await pending;
    return route.fulfill({
      status: 401,
      json: { error: { message: "Sign in" } },
    });
  });
  await page.goto("/");
  await expect(page.getByRole("status")).toContainText("Checking your session");
  await expect(page.getByRole("navigation")).toHaveCount(0);
  await expect(page.locator(".sidebar")).toHaveCount(0);
  expect(requests).toEqual(["/v1/account"]);
  release();
  await expect(
    page.getByRole("link", { name: "Continue with moeSegFault" }),
  ).toBeVisible();
  expect(requests).toEqual(["/v1/account"]);
});

test("an expired authenticated request returns to the public entry and preserves the destination", async ({
  page,
}) => {
  await page.route("**/v1/**", (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/v1/account")
      return route.fulfill({
        json: { account_id: "owned", plan: "free", csrf_token: "fixture-csrf" },
      });
    if (path === "/v1/skills")
      return route.fulfill({
        status: 401,
        json: { error: { message: "Expired" } },
      });
    return route.fulfill({ json: { data: [] } });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Skills", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "Continue with moeSegFault" }),
  ).toHaveAttribute("href", "/auth/login?return_to=/%23skills");
  await expect(page.getByRole("navigation")).toHaveCount(0);
  await expect(page.getByRole("status")).toHaveText(
    "Your session has ended. Sign in to continue.",
  );
});
