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
import { Agents } from "./Agents";
import i18n from "./i18n";
import type { Agent, ModelRegistry, ProviderKey } from "./api";
const openrouter = {
  provider: "openrouter",
  model: "openai/example",
  context_window: 200000,
  max_output_tokens: 8192,
  platform_available: true,
};
const deepseek = {
  provider: "deepseek",
  model: "deepseek-flash",
  context_window: 1000000,
  max_output_tokens: 16384,
  capability_flag: "byok_only",
};
const registry: ModelRegistry = {
  data: [],
  available_models: [openrouter, deepseek],
  available_platform_models: [openrouter],
};
const keys: ProviderKey[] = [
  { id: "cred-deepseek", provider: "deepseek" },
  { id: "cred-openrouter", provider: "openrouter" },
];
const existing: Agent = {
  id: "agent-1",
  name: "Notebook",
  model: openrouter,
  instructions: "Pinned directions",
  compaction_percent: 80,
  workspace: true,
  skills: [],
};
let writes: { path: string; method: string; body: any }[] = [];
function mount(
  models: ModelRegistry | Promise<ModelRegistry> = registry,
  agents: Agent[] = [],
  credentials: ProviderKey[] | Promise<ProviderKey[]> = keys,
) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      const method = init.method ?? "GET";
      if (method !== "GET") {
        const body = JSON.parse(init.body as string);
        writes.push({ path, method, body });
        return new Response(JSON.stringify({ ...existing, ...body }));
      }
      const value =
        path === "/v1/agents"
          ? { data: agents }
          : path === "/v1/models"
            ? await models
            : path === "/v1/provider-keys"
              ? { data: await credentials }
              : path === "/v1/skills"
                ? { data: [] }
                : undefined;
      return new Response(
        JSON.stringify(
          value ?? { error: { message: "Unexpected test route" } },
        ),
        { status: value ? 200 : 404 },
      );
    }),
  );
  render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: {
            queries: { retry: false },
            mutations: { retry: false },
          },
        })
      }
    >
      <Agents />
    </QueryClientProvider>,
  );
}
async function createForm() {
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "New agent" }));
  const dialog = within(screen.getByRole("dialog"));
  await user.type(dialog.getByLabelText("Name"), "Research");
  await waitFor(() =>
    expect(dialog.getByRole("button", { name: "Save agent" })).toBeEnabled(),
  );
  return {
    user,
    dialog,
    provider: dialog.getByRole("combobox", { name: "Provider" }),
    model: dialog.getByRole("combobox", { name: "Model" }),
  };
}
function options() {
  return [
    ...document.querySelectorAll<HTMLDataListElement>("#models option"),
  ].map((option) => option.getAttribute("value"));
}
beforeEach(() => {
  writes = [];
  i18n.changeLanguage("en");
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});
describe("metadata-driven model selection", () => {
  it("offers only priced platform providers and rejects a BYOK-only model typed manually", async () => {
    mount();
    const { user, dialog, provider, model } = await createForm();
    expect(
      within(provider as HTMLElement).queryByRole("option", {
        name: "DeepSeek",
      }),
    ).not.toBeInTheDocument();
    expect(options()).toEqual(["openai/example"]);
    await user.type(model, "deepseek-flash");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Choose a registered model",
    );
    expect(writes).toEqual([]);
  });
  it("uses the selected DeepSeek key namespace for BYOK and sends only model domain fields", async () => {
    mount();
    const { user, dialog, provider, model } = await createForm();
    await user.selectOptions(provider, "byok");
    expect(model).toBeDisabled();
    expect(options()).toEqual([]);
    await user.selectOptions(
      await dialog.findByRole("combobox", { name: "Saved API key" }),
      "cred-deepseek",
    );
    await waitFor(() => expect(model).toBeEnabled());
    expect(options()).toEqual(["deepseek-flash"]);
    await user.type(model, "deepseek-flash");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual({
      provider: "byok",
      model: "deepseek-flash",
      context_window: 1000000,
      max_output_tokens: 16384,
      credential_id: "cred-deepseek",
    });
  });
  it("accepts custom BYOK model IDs without requiring a platform registry entry", async () => {
    mount();
    const { user, dialog, provider, model } = await createForm();
    await user.selectOptions(provider, "byok");
    await user.selectOptions(
      await dialog.findByRole("combobox", { name: "Saved API key" }),
      "cred-deepseek",
    );
    await waitFor(() => expect(model).toBeEnabled());
    await user.type(model, "customer/private-model");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual({
      provider: "byok",
      model: "customer/private-model",
      context_window: 131072,
      max_output_tokens: 8192,
      credential_id: "cred-deepseek",
    });
  });
  it("clears incompatible models on provider/key switches but keeps valid selections", async () => {
    mount();
    const { user, dialog, provider, model } = await createForm();
    await user.type(model, "openai/example");
    await user.selectOptions(provider, "byok");
    expect(model).toHaveValue("");
    const credential = await dialog.findByRole("combobox", {
      name: "Saved API key",
    });
    await user.selectOptions(credential, "cred-openrouter");
    await waitFor(() => expect(model).toBeEnabled());
    await user.type(model, "openai/example");
    await user.selectOptions(provider, "openrouter");
    expect(model).toHaveValue("openai/example");
    await user.selectOptions(provider, "byok");
    expect(model).toHaveValue("openai/example");
    await user.selectOptions(
      dialog.getByRole("combobox", { name: "Saved API key" }),
      "cred-deepseek",
    );
    expect(model).toHaveValue("");
    expect(options()).toEqual(["deepseek-flash"]);
  });
  it("treats an explicit empty platform subset as no platform models, not legacy fallback", async () => {
    mount({ ...registry, available_platform_models: [] });
    const { dialog, provider } = await createForm();
    expect(
      within(provider as HTMLElement).queryByRole("option", {
        name: "OpenRouter",
      }),
    ).not.toBeInTheDocument();
    expect(
      within(provider as HTMLElement).queryByRole("option", {
        name: "DeepSeek",
      }),
    ).not.toBeInTheDocument();
    expect(options()).toEqual([]);
    expect(dialog.getByRole("combobox", { name: "Model" })).toBeDisabled();
  });
  it("supports old servers without permanently disabling DeepSeek platform availability", async () => {
    mount({ data: [], available_models: [deepseek] });
    const { user, dialog, provider, model } = await createForm();
    expect(provider).toHaveValue("deepseek");
    expect(options()).toEqual(["deepseek-flash"]);
    await user.type(model, "deepseek-flash");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual({
      provider: "deepseek",
      model: "deepseek-flash",
      context_window: 1000000,
      max_output_tokens: 16384,
    });
  });
  it("preserves an existing model while metadata loads and only name/instructions are edited", async () => {
    let resolve!: (data: ModelRegistry) => void;
    const pending = new Promise<ModelRegistry>((done) => {
      resolve = done;
    });
    mount(pending, [existing]);
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Edit Notebook" }),
    );
    const dialog = within(screen.getByRole("dialog"));
    expect(dialog.getByRole("combobox", { name: "Model" })).toHaveValue(
      "openai/example",
    );
    await user.type(dialog.getByLabelText("Name"), " updated");
    await user.type(dialog.getByLabelText("Instructions"), " updated");
    resolve(registry);
    await waitFor(() =>
      expect(dialog.getByRole("button", { name: "Save agent" })).toBeEnabled(),
    );
    expect(dialog.getByRole("combobox", { name: "Model" })).toHaveValue(
      "openai/example",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0]).toMatchObject({
      method: "PATCH",
      body: {
        name: "Notebook updated",
        instructions: "Pinned directions updated",
        model: { provider: "openrouter", model: "openai/example" },
      },
    });
    expect(writes[0].body.model).not.toHaveProperty("platform_available");
  });
  it("preserves an existing BYOK model while its saved key metadata loads", async () => {
    let resolve!: (data: ProviderKey[]) => void;
    const pending = new Promise<ProviderKey[]>((done) => {
      resolve = done;
    });
    mount(
      registry,
      [
        {
          ...existing,
          model: {
            ...deepseek,
            provider: "byok",
            credential_id: "cred-deepseek",
          },
        },
      ],
      pending,
    );
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Edit Notebook" }),
    );
    const dialog = within(screen.getByRole("dialog"));
    expect(dialog.getByRole("combobox", { name: "Model" })).toHaveValue(
      "deepseek-flash",
    );
    await user.type(dialog.getByLabelText("Name"), " updated");
    resolve(keys);
    await waitFor(() =>
      expect(dialog.getByRole("button", { name: "Save agent" })).toBeEnabled(),
    );
    expect(dialog.getByRole("combobox", { name: "Model" })).toHaveValue(
      "deepseek-flash",
    );
    expect(options()).toEqual(["deepseek-flash"]);
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual({
      provider: "byok",
      model: "deepseek-flash",
      context_window: 1000000,
      max_output_tokens: 16384,
      credential_id: "cred-deepseek",
    });
  });
  it("publishes a generic Anthropic connection with explicit custom model limits", async () => {
    mount(
      registry,
      [],
      [
        {
          id: "anthropic-owned",
          protocol: "anthropic",
          name: "Research API",
          base_url: "https://custom.example/v1",
        },
      ],
    );
    const { user, dialog, provider, model } = await createForm();
    await user.selectOptions(provider, "byok");
    const key = await dialog.findByRole("combobox", { name: "Saved API key" });
    expect(
      within(key).getByRole("option", { name: "Research API" }),
    ).toBeInTheDocument();
    await user.selectOptions(key, "anthropic-owned");
    await waitFor(() => expect(model).toBeEnabled());
    expect(options()).toEqual([]);
    await user.type(model, "claude-custom-model");
    await user.click(dialog.getByText("Model limits", { exact: true }));
    const context = dialog.getByLabelText("Context window");
    const output = dialog.getByLabelText("Maximum output tokens");
    await user.clear(context);
    await user.type(context, "200000");
    await user.clear(output);
    await user.type(output, "16000");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual({
      provider: "byok",
      model: "claude-custom-model",
      context_window: 200000,
      max_output_tokens: 16000,
      credential_id: "anthropic-owned",
    });
  });
  it("preserves existing custom BYOK model limits when editing instructions", async () => {
    const model = {
      provider: "byok",
      model: "custom/not-in-registry",
      context_window: 262144,
      max_output_tokens: 16384,
      credential_id: "generic-owned",
    };
    mount(
      registry,
      [{ ...existing, model }],
      [
        {
          id: "generic-owned",
          protocol: "openai",
          name: "Private API",
          base_url: "https://private.example/v1",
        },
      ],
    );
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Edit Notebook" }),
    );
    const dialog = within(screen.getByRole("dialog"));
    await waitFor(() =>
      expect(dialog.getByRole("button", { name: "Save agent" })).toBeEnabled(),
    );
    await user.type(dialog.getByLabelText("Instructions"), " updated");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].body.model).toEqual(model);
  });
});
