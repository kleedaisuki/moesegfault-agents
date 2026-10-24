import { expect, test } from "@playwright/test";

test("same-tab subscription return opens usage with fresh account and wallet data", async ({
  page,
}) => {
  let tier = "free";
  let accountReads = 0;
  let usageReads = 0;
  await page.addInitScript(() => {
    localStorage.setItem("magents.language", "en");
  });
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/v1/account") {
      accountReads++;
      return route.fulfill({
        json: {
          account_id: "subscription-return-fixture",
          plan: tier,
          csrf_token: "fixture-csrf",
          limits: { agents: tier === "go" ? 10 : 2, concurrent_sessions: 2 },
          subscription_url:
            "https://subscribe-staging.moesegfault.dev/?app=agents&return_to=https%3A%2F%2Fadash-staging.moesegfault.dev%2Fbilling%2Freturn",
        },
      });
    }
    if (path === "/v1/usage") {
      usageReads++;
      return route.fulfill({
        json: {
          wallet: {
            budget_microusd: tier === "go" ? "8000000" : "1000000",
            available_microusd: tier === "go" ? "8000000" : "1000000",
            spent_microusd: "0",
            reserved_microusd: "0",
          },
          by_resource: [],
        },
      });
    }
    return route.fulfill({ json: { data: [] } });
  });
  await page.route("https://subscribe-staging.moesegfault.dev/**", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<a href="http://127.0.0.1:5173/billing/return">Return to Agents</a>',
    }),
  );
  await page.route("**/billing/return", (route) => {
    tier = "go";
    return route.fulfill({ status: 302, headers: { Location: "/#usage" } });
  });
  await page.goto("/#usage");
  await expect(
    page.getByRole("heading", { name: "Usage & plan" }),
  ).toBeVisible();
  await expect(page.locator(".plan h2")).toHaveText("free");
  const accountBefore = accountReads;
  const usageBefore = usageReads;
  const manage = page.getByRole("link", { name: "Manage subscription" });
  await expect(manage).not.toHaveAttribute("target");
  await manage.click();
  await page.getByRole("link", { name: "Return to Agents" }).click();
  await expect(page).toHaveURL("http://127.0.0.1:5173/#usage");
  await expect(page.locator(".plan h2")).toHaveText("go");
  await expect(page.locator(".plan")).toContainText("$8.00");
  expect(accountReads).toBeGreaterThan(accountBefore);
  expect(usageReads).toBeGreaterThan(usageBefore);
});

test("refresh failure remains visible on the returned usage page", async ({
  page,
}) => {
  await page.addInitScript(() =>
    localStorage.setItem("magents.language", "en"),
  );
  await page.route("**/v1/**", (route) =>
    route.fulfill({
      json:
        new URL(route.request().url()).pathname === "/v1/account"
          ? {
              account_id: "fixture",
              plan: "go",
              limits: {},
              csrf_token: "csrf",
            }
          : { wallet: {}, by_resource: [] },
    }),
  );
  await page.goto("/?billing_refresh=failed#usage");
  await expect(page.getByRole("alert")).toContainText("could not be refreshed");
  await expect(
    page.getByRole("link", { name: "Retry subscription refresh" }),
  ).toHaveAttribute("href", "/billing/return");
});

