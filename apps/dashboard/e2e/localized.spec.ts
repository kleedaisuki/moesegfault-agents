import { test, expect } from "@playwright/test";
import en from "../src/locales/en.json" with { type: "json" };
import zh from "../src/locales/zh-CN.json" with { type: "json" };
const cases = [
  { language: "en", theme: "light", width: 1280, plan: "free" },
  { language: "zh-CN", theme: "dark", width: 1280, plan: "go" },
  { language: "zh-CN", theme: "light", width: 320, plan: "plus" },
  { language: "en", theme: "dark", width: 360, plan: "pro" },
] as const;
for (const scenario of cases)
  test(`${scenario.language} ${scenario.theme} ${scenario.width}px full management flow`, async ({
    page,
  }) => {
    const strings = scenario.language === "en" ? en : zh;
    const t = (key: string) => (strings as Record<string, string>)[key] ?? key;
    await page.setViewportSize({ width: scenario.width, height: 900 });
    await page.addInitScript(({ language, theme }) => {
      if (!localStorage.getItem("magents.language"))
        localStorage.setItem("magents.language", language);
      if (!localStorage.getItem("magents.theme"))
        localStorage.setItem("magents.theme", theme);
    }, scenario);
    const agent = {
      id: "agent-matrix",
      name: "Notebook",
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
    let skills: {
      name: string;
      instructions: string;
      version: string;
      source: string;
    }[] = [];
    let messages: { role: string; content: string }[] = [];
    let keyCreated = false;
    let providerSaved = false;
    let messageCalls = 0;
    let accountCalls = 0;
    await page.route("**/v1/**", async (route) => {
      const request = route.request();
      const path = new URL(request.url()).pathname;
      const method = request.method();
      if (path === "/v1/account") {
        accountCalls++;
        return route.fulfill({
          json: {
            account_id: "fixture",
            plan: scenario.plan,
            billing_mode: "test",
            csrf_token: "fixture-csrf",
            limits: { agents: 30, concurrent_sessions: 15 },
            credit_microusd: 18000000,
            subscription_url: "https://subscribe-staging.moesegfault.dev/",
          },
        });
      }
      if (method !== "GET")
        expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      if (path === "/v1/skills" && method === "POST") {
        skills = [
          { ...request.postDataJSON(), version: "v1", source: "account" },
        ];
        return route.fulfill({ status: 201, json: skills[0] });
      }
      if (path === "/v1/skills")
        return route.fulfill({ json: { data: skills } });
      if (path === "/v1/models")
        return route.fulfill({
          json: { data: [], available_models: [agent.model] },
        });
      if (path === "/v1/agents" && method === "POST") {
        agents = [agent];
        return route.fulfill({ status: 201, json: agent });
      }
      if (path === "/v1/agents")
        return route.fulfill({ json: { data: agents } });
      if (path === "/v1/agents/agent-matrix/sessions")
        return route.fulfill({
          json:
            method === "POST"
              ? { id: "session-matrix", agent_id: agent.id, status: "idle" }
              : { data: [] },
        });
      if (path === "/v1/sessions/session-matrix/messages") {
        messageCalls++;
        messages.push(
          {
            role: "user",
            content:
              request.postDataJSON().content ??
              request.postDataJSON().messages?.[0]?.content,
          },
          { role: "assistant", content: "Done." },
        );
        if (request.postDataJSON().stream)
          return route.fulfill({
            contentType: "text/event-stream",
            body:
              "data: " +
              JSON.stringify({ choices: [{ delta: { content: "Done." } }] }) +
              "\n\ndata: [DONE]\n\n",
          });
        return route.fulfill({
          json: { queued: true, input_revision: messageCalls },
        });
      }
      if (path === "/v1/sessions/session-matrix")
        return route.fulfill({
          json: {
            id: "session-matrix",
            agent_id: agent.id,
            status: messageCalls ? "running" : "idle",
            input_revision: messageCalls,
            last_input_at_ms: Date.now(),
            context_tokens: 1000,
            messages,
          },
        });
      if (path === "/v1/api-keys" && method === "POST") {
        keyCreated = true;
        return route.fulfill({
          json: { id: "key-1", key: "mag_fixture_key_only" },
        });
      }
      if (path === "/v1/api-keys")
        return route.fulfill({
          json: {
            data: keyCreated
              ? [
                  {
                    id: "key-1",
                    name: "Notebook",
                    prefix: "mag_fixture",
                    created_at: 1792764000,
                  },
                ]
              : [],
          },
        });
      if (path === "/v1/provider-keys" && method === "POST") {
        expect(request.postDataJSON()).toMatchObject({
          protocol: "openai",
          base_url: "https://api.deepseek.com/v1",
          api_key: "fixture-provider-key",
        });
        providerSaved = true;
        return route.fulfill({
          json: {
            id: "cred-1",
            protocol: "openai",
            base_url: "https://api.deepseek.com/v1",
            name: "Personal endpoint",
          },
        });
      }
      if (path === "/v1/provider-keys")
        return route.fulfill({
          json: {
            data: providerSaved
              ? [
                  {
                    id: "cred-1",
                    protocol: "openai",
                    base_url: "https://api.deepseek.com/v1",
                    name: "Personal endpoint",
                  },
                ]
              : [],
          },
        });
      if (path === "/v1/diagnostics")
        return route.fulfill({ json: { accepted: true } });
      if (path === "/v1/usage")
        return route.fulfill({
          json: {
            wallet: {
              budget_microusd: "18000000",
              spent_microusd: "1000000",
              reserved_microusd: "1000",
              available_microusd: "16999000",
            },
            by_resource: [
              { resource: "model", cost_microusd: "1000000" },
              { resource: "container", cost_microusd: "1" },
            ],
          },
        });
      return route.fulfill({
        status: 404,
        json: { error: { message: "Missing fixture" } },
      });
    });
    await page.goto("/");
    await expect(page.locator("html")).toHaveAttribute(
      "data-theme",
      scenario.theme,
    );
    await expect(page.locator("html")).toHaveAttribute(
      "lang",
      scenario.language,
    );
    await expect(page.getByLabel(t("Language"))).toBeVisible();
    await expect(page.getByLabel(t("Appearance"))).toBeVisible();
    await page.getByRole("button", { name: t("Skills"), exact: true }).click();
    await page
      .getByRole("button", { name: t("New skill"), exact: true })
      .click();
    await page
      .getByRole("dialog")
      .getByLabel(t("Name"), { exact: true })
      .fill("review");
    await page
      .getByRole("dialog")
      .getByLabel(t("Instructions"), { exact: true })
      .fill("Review carefully.");
    await page
      .getByRole("button", { name: t("Save skill"), exact: true })
      .click();
    await expect(page.getByText("review", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: t("Agents"), exact: true }).click();
    await page
      .getByRole("button", { name: t("New agent"), exact: true })
      .click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel(t("Name"), { exact: true }).fill("Notebook");
    await dialog
      .getByRole("combobox", { name: t("Provider"), exact: true })
      .selectOption("deepseek");
    await dialog
      .getByRole("combobox", { name: t("Model"), exact: true })
      .fill("deepseek-flash");
    await dialog.getByRole("checkbox", { name: "review", exact: true }).check();
    await dialog
      .getByRole("button", { name: t("Save agent"), exact: true })
      .click();
    await expect(dialog).not.toBeVisible();
    await page.getByRole("button", { name: "Notebook", exact: true }).click();
    await page
      .getByRole("button", { name: t("Start a session"), exact: true })
      .click();
    await page
      .getByLabel(t("Message or steering instruction"))
      .fill("Review tests");
    await page.getByRole("button", { name: t("Send"), exact: true }).click();
    await expect(page.getByText("Done.", { exact: true })).toBeVisible();
    await expect(page.getByText("Review tests", { exact: true })).toBeVisible();
    await page
      .getByLabel(t("Message or steering instruction"))
      .fill("Focus on correctness");
    await page.getByRole("button", { name: t("Send"), exact: true }).click();
    await expect.poll(() => messageCalls).toBe(2);
    await page.getByRole("button", { name: t("Access"), exact: true }).click();
    await page.getByLabel(t("New API key name")).fill("Notebook");
    await page
      .getByRole("button", { name: t("Create key"), exact: true })
      .click();
    await expect(
      page.getByLabel(t("New API key"), { exact: true }),
    ).toHaveValue("mag_fixture_key_only");
    await page
      .getByRole("button", { name: t("I’ve saved it"), exact: true })
      .click();
    await page
      .getByLabel(t("API type"), { exact: true })
      .selectOption("openai");
    await page.getByLabel(t("Base URL")).fill("https://api.deepseek.com/v1");
    await page.getByLabel(t("Provider API key")).fill("fixture-provider-key");
    await page
      .getByRole("button", { name: t("Save key"), exact: true })
      .click();
    await page
      .getByRole("checkbox", {
        name: t("I authorize sending this diagnostic payload."),
      })
      .check();
    await page
      .getByRole("button", { name: t("Send diagnostics"), exact: true })
      .click();
    await expect(page.getByRole("status")).toHaveText(
      t("Diagnostic received."),
    );
    await page
      .getByRole("button", { name: t("Usage & plan"), exact: true })
      .click();
    await expect(
      page.getByRole("heading", { name: t(scenario.plan), exact: true }),
    ).toBeVisible();
    await expect(
      page.getByText(t("Test credits · No real charges"), { exact: true }),
    ).toBeVisible();
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    const targets = await page
      .locator("button:visible,select:visible")
      .evaluateAll((elements) =>
        elements.map((el) => ({
          label: el.getAttribute("aria-label") ?? el.textContent,
          height: el.getBoundingClientRect().height,
        })),
      );
    expect(targets.filter((target) => target.height < 43.9)).toEqual([]);
    expect(accountCalls).toBe(1);
    await page.screenshot({
      path: `../../.cache/dashboard-${scenario.language}-${scenario.theme}-${scenario.width}.png`,
      fullPage: true,
    });
    const stored = await page.evaluate(() =>
      Object.fromEntries(Object.entries(localStorage)),
    );
    expect(Object.keys(stored).sort()).toEqual([
      "magents.language",
      "magents.theme",
    ]);
    expect(JSON.stringify(stored)).not.toContain("fixture");
    await page.getByLabel(t("Appearance")).selectOption("system");
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await page.emulateMedia({ colorScheme: "light" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    const otherLanguage = scenario.language === "en" ? "zh-CN" : "en";
    await page.getByLabel(t("Language")).selectOption(otherLanguage);
    await expect(page.locator("html")).toHaveAttribute("lang", otherLanguage);
    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("lang", otherLanguage);
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });
