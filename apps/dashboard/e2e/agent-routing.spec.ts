import { test, expect } from "@playwright/test";
import en from "../src/locales/en.json" with { type: "json" };
import zh from "../src/locales/zh-CN.json" with { type: "json" };
import type { Agent } from "../src/api";

for (const scenario of [
  { language: "en", theme: "light", width: 320 },
  { language: "zh-CN", theme: "dark", width: 390 },
] as const) {
  test(`${scenario.language} ${scenario.theme} ${scenario.width}px independent tool routing`, async ({
    page,
  }, testInfo) => {
    const strings = scenario.language === "en" ? en : zh;
    const t = (key: string) => (strings as Record<string, string>)[key] ?? key;
    await page.setViewportSize({ width: scenario.width, height: 844 });
    await page.addInitScript(({ language, theme }) => {
      localStorage.setItem("magents.language", language);
      localStorage.setItem("magents.theme", theme);
    }, scenario);
    let agent: Agent = {
      id: "native-routing",
      name: "Notebook",
      model: {
        provider: "byok",
        model: "private/native-model",
        context_window: 131072,
        max_output_tokens: 8192,
        credential_id: "llm-key",
        protocol: "responses",
      },
      instructions: "Pinned",
      compaction_percent: 80,
      workspace: true,
      skills: [],
    };
    const originalModel = agent.model;
    let writes = 0;
    let keyReads = 0;
    await page.route("**/v1/**", async (route) => {
      const request = route.request();
      const path = new URL(request.url()).pathname;
      if (path === "/v1/account")
        return route.fulfill({
          json: {
            account_id: "fixture",
            plan: "free",
            csrf_token: "routing-csrf",
          },
        });
      if (path === "/v1/agents")
        return route.fulfill({ json: { data: [agent] } });
      if (path === "/v1/models")
        return route.fulfill({
          json: {
            data: [],
            available_models: [],
            available_platform_models: [],
          },
        });
      if (path === "/v1/skills") return route.fulfill({ json: { data: [] } });
      if (path === "/v1/provider-keys") {
        expect(request.method()).toBe("GET");
        keyReads++;
        return route.fulfill({
          json: {
            data: [
              { id: "llm-key", name: "Language model", protocol: "openai" },
              { id: "image-key", name: "Image route", protocol: "openai" },
              { id: "search-key", name: "Search route", protocol: "openai" },
            ],
          },
        });
      }
      if (path === "/v1/agents/native-routing") {
        expect(request.method()).toBe("PATCH");
        expect(request.headers()["x-csrf-token"]).toBe("routing-csrf");
        const body = request.postDataJSON();
        expect(body).not.toHaveProperty("model");
        expect({ ...agent, ...body }.model).toEqual(originalModel);
        expect(body.tool_services).toEqual({
          image_generation: {
            provider: "openai_images",
            credential_id: "image-key",
          },
          web_search: {
            provider: "deepseek_search",
            credential_id: "search-key",
          },
        });
        expect(body).not.toHaveProperty("api_key");
        expect(body).not.toHaveProperty("base_url");
        agent = { ...agent, ...body };
        writes++;
        return route.fulfill({ json: agent });
      }
      return route.fulfill({
        status: 404,
        json: { error: { message: "Unexpected test route" } },
      });
    });
    await page.goto("/");
    await expect(page.locator("html")).toHaveAttribute(
      "data-theme",
      scenario.theme,
    );
    await page
      .getByRole("button", {
        name: t("Edit {{name}}").replace("{{name}}", "Notebook"),
      })
      .click();
    const dialog = page.getByRole("dialog");
    await expect(
      dialog.getByRole("button", { name: t("Save agent"), exact: true }),
    ).toBeEnabled();
    await dialog
      .getByLabel(t("Name"), { exact: true })
      .fill("Notebook updated");
    await expect(
      dialog.getByRole("combobox", { name: t("API transport"), exact: true }),
    ).toBeHidden();
    await dialog.locator("summary", { hasText: t("Model transport") }).click();
    await expect(
      dialog.getByRole("combobox", { name: t("API transport"), exact: true }),
    ).toHaveValue("responses");
    await dialog.locator("summary", { hasText: t("Model transport") }).click();
    await dialog.locator("summary", { hasText: t("Tool services") }).click();
    await dialog
      .getByRole("combobox", { name: t("Image service"), exact: true })
      .selectOption("openai_images");
    await dialog
      .getByRole("combobox", { name: t("Image API key"), exact: true })
      .selectOption("image-key");
    await dialog
      .getByRole("combobox", { name: t("Search service"), exact: true })
      .selectOption("deepseek_search");
    await dialog
      .getByRole("combobox", { name: t("Search API key"), exact: true })
      .selectOption("search-key");
    await expect(
      dialog.getByLabel(t("Search model"), { exact: true }),
    ).toHaveCount(0);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    expect(
      await dialog
        .locator(".modal")
        .evaluate((element) => element.scrollWidth <= element.clientWidth),
    ).toBe(true);
    await page.screenshot({
      path: testInfo.outputPath("routing-mobile.png"),
      fullPage: true,
    });
    await dialog
      .getByRole("button", { name: t("Save agent"), exact: true })
      .click();
    await expect(dialog).not.toBeVisible();
    await expect(
      page.getByRole("button", { name: "Notebook updated", exact: true }),
    ).toBeVisible();
    expect(writes).toBe(1);
    expect(keyReads).toBe(1);
  });
}
