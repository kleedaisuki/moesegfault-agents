import { test, expect } from "@playwright/test";

test("multiple custom endpoints and a non-registry Anthropic model remain usable from the browser", async ({
  page,
}) => {
  const connections: {
    id: string;
    protocol: string;
    base_url: string;
    name: string;
  }[] = [];
  let published = false;
  await page.route("**/v1/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    const method = request.method();
    if (path === "/v1/account")
      return route.fulfill({
        json: { account_id: "owned", plan: "free", csrf_token: "owned-csrf" },
      });
    if (method !== "GET")
      expect(request.headers()["x-csrf-token"]).toBe("owned-csrf");
    if (path === "/v1/provider-keys" && method === "POST") {
      const { api_key, ...metadata } = request.postDataJSON();
      expect(api_key).toBe("synthetic-browser-only-key");
      const credential = {
        id: `credential-${connections.length}`,
        ...metadata,
      };
      connections.push(credential);
      return route.fulfill({ status: 201, json: credential });
    }
    if (path === "/v1/provider-keys")
      return route.fulfill({ json: { data: connections } });
    if (path === "/v1/agents" && method === "POST") {
      const body = request.postDataJSON();
      expect(body.model).toEqual({
        provider: "byok",
        credential_id: "credential-2",
        model: "private/claude-model",
        context_window: 200000,
        max_output_tokens: 16000,
      });
      expect(JSON.stringify(body)).not.toContain("synthetic-browser-only-key");
      expect(JSON.stringify(body)).not.toContain("base_url");
      published = true;
      return route.fulfill({ status: 201, json: { id: "published", ...body } });
    }
    if (path === "/v1/models")
      return route.fulfill({
        json: { data: [], available_models: [], available_platform_models: [] },
      });
    return route.fulfill({ json: { data: [] } });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Access", exact: true }).click();
  for (const [index, protocol] of ["openai", "openai", "anthropic"].entries()) {
    await page.getByLabel("API type", { exact: true }).selectOption(protocol);
    await page
      .getByLabel("Connection name (optional)")
      .fill(`Endpoint ${index}`);
    await page
      .getByLabel("Base URL", { exact: true })
      .fill(`https://api-${index}.example/v1`);
    await page
      .getByLabel("Provider API key", { exact: true })
      .fill("synthetic-browser-only-key");
    await page.getByRole("button", { name: "Save key", exact: true }).click();
    await expect(
      page.getByLabel("Provider API key", { exact: true }),
    ).toHaveValue("");
    await expect(
      page.getByText(`Endpoint ${index}`, { exact: true }),
    ).toBeVisible();
  }
  expect(connections.map((connection) => connection.protocol)).toEqual([
    "openai",
    "openai",
    "anthropic",
  ]);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  await page.getByRole("button", { name: "New agent", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name", { exact: true }).fill("Custom agent");
  await dialog
    .getByRole("combobox", { name: "Provider", exact: true })
    .selectOption("byok");
  await dialog
    .getByRole("combobox", { name: "Saved API key", exact: true })
    .selectOption("credential-2");
  await dialog
    .getByRole("combobox", { name: "Model", exact: true })
    .fill("private/claude-model");
  await dialog.getByText("Model limits", { exact: true }).click();
  await dialog.getByLabel("Context window", { exact: true }).fill("200000");
  await dialog
    .getByLabel("Maximum output tokens", { exact: true })
    .fill("16000");
  await dialog.getByRole("button", { name: "Save agent", exact: true }).click();
  await expect(dialog).not.toBeVisible();
  expect(published).toBe(true);
  expect(
    await page.evaluate(() => JSON.stringify({ ...localStorage })),
  ).not.toContain("synthetic-browser-only-key");
});
