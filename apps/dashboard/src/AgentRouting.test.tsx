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
import type { Agent, Model, ProviderKey, ToolServices } from "./api";
import i18n from "./i18n";

const platformModel = {
  provider: "deepseek",
  model: "example-text",
  context_window: 131072,
  max_output_tokens: 8192,
};
const nativeAgent: Agent = {
  id: "agent-native",
  name: "Notebook",
  model: {
    ...platformModel,
    provider: "byok",
    credential_id: "llm-key",
    protocol: "responses",
  },
  instructions: "Pinned",
  compaction_percent: 80,
  workspace: true,
  skills: [],
};
const keys: ProviderKey[] = [
  {
    id: "llm-key",
    name: "Language model",
    protocol: "openai",
    provider: "deepseek",
  },
  {
    id: "image-key",
    name: "Image service",
    protocol: "openai",
    provider: "openrouter",
  },
  { id: "search-key", name: "Search service", protocol: "openai" },
  { id: "anthropic-key", name: "Anthropic only", protocol: "anthropic" },
];
let writes: {
  model?: Agent["model"];
  tool_services?: ToolServices;
  name: string;
}[];
let reads: string[];
function mount(
  agent: Agent | null = nativeAgent,
  registeredModel: Model = platformModel,
  credentials: ProviderKey[] | Promise<ProviderKey[]> = keys,
) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      if (init.method === "PATCH" || init.method === "POST") {
        const body = JSON.parse(init.body as string);
        writes.push(body);
        return Response.json({ ...agent, ...body });
      }
      reads.push(path);
      if (path === "/v1/agents")
        return Response.json({ data: agent ? [agent] : [] });
      if (path === "/v1/models")
        return Response.json({
          data: [],
          available_models: [registeredModel],
          available_platform_models: [registeredModel],
        });
      if (path === "/v1/skills") return Response.json({ data: [] });
      if (path === "/v1/provider-keys")
        return Response.json({ data: await credentials });
      throw new Error(`Unexpected route ${path}`);
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
async function edit() {
  const user = userEvent.setup();
  await user.click(
    await screen.findByRole("button", { name: "Edit Notebook" }),
  );
  const dialog = within(screen.getByRole("dialog"));
  await waitFor(() =>
    expect(dialog.getByRole("button", { name: "Save agent" })).toBeEnabled(),
  );
  return { user, dialog };
}
beforeEach(async () => {
  writes = [];
  reads = [];
  await i18n.changeLanguage("en");
});
afterEach(async () => {
  cleanup();
  vi.unstubAllGlobals();
  await i18n.changeLanguage("en");
});

describe("independent agent service routing", () => {
  it.each(["responses", "openai", "anthropic", undefined] as const)(
    "renaming retains transport %s and untouched service configuration",
    async (protocol) => {
      const agent: Agent = {
        ...nativeAgent,
        model: {
          ...nativeAgent.model,
          protocol,
          credential_id: protocol === "anthropic" ? "anthropic-key" : "llm-key",
        },
        tool_services: {
          image_generation: {
            provider: "openai_images",
            credential_id: "image-key",
            model: "image-model",
            asset_hosts: ["cdn.example.com"],
          },
          web_search: {
            provider: "openai_responses",
            credential_id: "search-key",
            model: "search-model",
            max_tool_calls: 2,
            max_output_tokens: 2048,
          },
        },
      };
      mount(agent);
      const { user, dialog } = await edit();
      await user.type(dialog.getByLabelText("Name"), " renamed");
      await user.click(dialog.getByRole("button", { name: "Save agent" }));
      await waitFor(() => expect(writes).toHaveLength(1));
      expect(writes[0]).not.toHaveProperty("model");
      expect({ ...agent, ...writes[0] }.model).toEqual(agent.model);
      expect(writes[0]).not.toHaveProperty("tool_services");
      expect({ ...agent, ...writes[0] }.tool_services).toEqual(
        agent.tool_services,
      );
    },
  );

  it.each([false, true])(
    "adopts a native registry preset for new selection unless transport was explicitly chosen (%s)",
    async (explicit) => {
      mount(null, { ...platformModel, protocol: "responses" });
      const user = userEvent.setup();
      await user.click(
        await screen.findByRole("button", { name: "New agent" }),
      );
      const dialog = within(screen.getByRole("dialog"));
      await user.type(dialog.getByLabelText("Name"), "New native agent");
      await waitFor(() =>
        expect(
          dialog.getByRole("button", { name: "Save agent" }),
        ).toBeEnabled(),
      );
      if (explicit) {
        await user.click(dialog.getByText("Model transport", { exact: true }));
        await user.selectOptions(
          dialog.getByLabelText("API transport"),
          "openai",
        );
      }
      await user.type(
        dialog.getByRole("combobox", { name: "Model" }),
        platformModel.model,
      );
      await user.click(dialog.getByRole("button", { name: "Save agent" }));
      await waitFor(() => expect(writes).toHaveLength(1));
      expect(writes[0].model?.protocol).toBe(explicit ? "openai" : "responses");
    },
  );

  it("does not reinterpret a saved omitted protocol when matching native registry metadata loads", async () => {
    mount(
      { ...nativeAgent, model: platformModel },
      { ...platformModel, protocol: "responses" },
    );
    const { user, dialog } = await edit();
    await user.type(dialog.getByLabelText("Name"), " renamed");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0]).not.toHaveProperty("model");
    expect(
      { ...nativeAgent, model: platformModel, ...writes[0] }.model,
    ).toEqual(platformModel);
  });

  it("preserves future nested model fields on a name-only PATCH without requiring a matching saved key", async () => {
    const model = {
      ...nativeAgent.model,
      credential_id: "unlisted-key",
      route_policy: { version: 2, opaque: ["future", 7] },
    };
    const agent = { ...nativeAgent, model };
    mount(agent, platformModel, new Promise<ProviderKey[]>(() => {}));
    const { user, dialog } = await edit();
    expect(dialog.getByLabelText("Saved API key")).not.toBeRequired();
    await user.type(dialog.getByLabelText("Name"), " renamed");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0]).not.toHaveProperty("model");
    expect({ ...agent, ...writes[0] }.model).toEqual(model);
  });

  it("retains an existing explicit transport when a model preset has no transport override", async () => {
    mount();
    const { user, dialog } = await edit();
    const model = dialog.getByRole("combobox", { name: "Model" });
    await user.clear(model);
    await user.type(model, platformModel.model);
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].model?.protocol).toBe("responses");
  });

  it("uses separate image/search credentials without altering the LLM route or key authentication", async () => {
    mount();
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("Image service"),
      "openrouter_images",
    );
    await user.selectOptions(
      dialog.getByLabelText("Image API key"),
      "image-key",
    );
    await user.type(
      dialog.getByLabelText("Image model (optional)"),
      "owner/image-model",
    );
    await user.click(dialog.getByText("Image asset hosts", { exact: true }));
    await user.type(
      dialog.getByLabelText("Allowed asset hosts"),
      "one.example.com{Enter}two.example.com",
    );
    await user.selectOptions(
      dialog.getByLabelText("Search service"),
      "openai_responses",
    );
    await user.selectOptions(
      dialog.getByLabelText("Search API key"),
      "search-key",
    );
    expect(
      within(dialog.getByLabelText("Search API key")).queryByRole("option", {
        name: "Anthropic only",
      }),
    ).not.toBeInTheDocument();
    await user.type(dialog.getByLabelText("Search model"), "search-model");
    await user.click(dialog.getByText("Search limits", { exact: true }));
    await user.clear(dialog.getByLabelText("Search tool calls"));
    await user.type(dialog.getByLabelText("Search tool calls"), "2");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0]).not.toHaveProperty("model");
    expect({ ...nativeAgent, ...writes[0] }.model).toEqual(nativeAgent.model);
    expect(writes[0].tool_services).toEqual({
      image_generation: {
        provider: "openrouter_images",
        credential_id: "image-key",
        model: "owner/image-model",
        asset_hosts: ["one.example.com", "two.example.com"],
      },
      web_search: {
        provider: "openai_responses",
        credential_id: "search-key",
        model: "search-model",
        max_tool_calls: 2,
      },
    });
    expect(reads.filter((path) => path === "/v1/provider-keys")).toHaveLength(
      1,
    );
  });

  it("switches search services without leaking a previous route’s model or credential and retains image settings", async () => {
    const image = {
      provider: "openai_images" as const,
      credential_id: "image-key",
      asset_hosts: ["cdn.example.com"],
    };
    mount({
      ...nativeAgent,
      tool_services: {
        image_generation: image,
        web_search: {
          provider: "openai_responses",
          credential_id: "search-key",
          model: "search-model",
          max_output_tokens: 2048,
          max_tool_calls: 2,
        },
      },
    });
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("Search service"),
      "platform_deepseek",
    );
    expect(dialog.queryByLabelText("Search API key")).not.toBeInTheDocument();
    expect(dialog.queryByLabelText("Search model")).not.toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].tool_services).toEqual({
      image_generation: image,
      web_search: {
        provider: "platform_deepseek",
        max_output_tokens: 2048,
        max_tool_calls: 2,
      },
    });
  });

  it("can remove explicit routes to restore platform defaults", async () => {
    mount({
      ...nativeAgent,
      tool_services: {
        image_generation: { provider: "platform" },
        web_search: { provider: "platform_deepseek" },
      },
    });
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await user.selectOptions(dialog.getByLabelText("Image service"), "");
    await user.selectOptions(dialog.getByLabelText("Search service"), "");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].tool_services).toEqual({});
  });

  it("keeps default controls closed and key metadata lazy; selecting native transport does not change auth family", async () => {
    mount({ ...nativeAgent, model: platformModel });
    const { user, dialog } = await edit();
    expect(reads).not.toContain("/v1/provider-keys");
    expect(
      dialog.getByText("Tool services", { exact: true }).parentElement,
    ).not.toHaveAttribute("open");
    expect(
      dialog.getByText("Model transport", { exact: true }).parentElement,
    ).not.toHaveAttribute("open");
    await user.click(dialog.getByText("Model transport", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("API transport"),
      "responses",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].model).toEqual({
      ...platformModel,
      protocol: "responses",
    });
    expect(writes[0]).not.toHaveProperty("tool_services");
    expect(reads).not.toContain("/v1/provider-keys");
  });

  it("requires an explicit compatible transport after switching to an Anthropic key", async () => {
    mount();
    const { user, dialog } = await edit();
    await user.selectOptions(
      dialog.getByLabelText("Saved API key"),
      "anthropic-key",
    );
    await user.type(
      dialog.getByRole("combobox", { name: "Model" }),
      "private/anthropic-model",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    expect(await dialog.findByRole("alert")).toHaveTextContent(
      "The model transport must match the saved key’s authentication family.",
    );
    expect(writes).toHaveLength(0);
    await user.click(dialog.getByText("Model transport", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("API transport"),
      "anthropic",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].model).toMatchObject({
      protocol: "anthropic",
      credential_id: "anthropic-key",
      model: "private/anthropic-model",
    });
  });

  it("can return an explicit transport to the omitted provider default without changing the key", async () => {
    mount();
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Model transport", { exact: true }));
    await user.selectOptions(dialog.getByLabelText("API transport"), "");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].model).not.toHaveProperty("protocol");
    expect(writes[0].model?.credential_id).toBe("llm-key");
    expect(writes[0].model?.provider).toBe("byok");
  });

  it("localizes the new disclosure and dedicated DeepSeek search without inventing an LLM model", async () => {
    await i18n.changeLanguage("zh-CN");
    mount({ ...nativeAgent, model: platformModel });
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "编辑 Notebook" }),
    );
    const dialog = within(screen.getByRole("dialog"));
    await user.click(dialog.getByText("工具服务", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("搜索服务"),
      "deepseek_search",
    );
    await user.selectOptions(
      await dialog.findByLabelText("搜索 API 密钥"),
      "llm-key",
    );
    expect(dialog.queryByLabelText("搜索模型")).not.toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "保存智能体" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].tool_services).toEqual({
      web_search: { provider: "deepseek_search", credential_id: "llm-key" },
    });
  });
});

describe("Code executor management increment", () => {
  it("Code executor chooses and validates its own existing credential without changing the model", async () => {
    mount(nativeAgent, platformModel, [
      ...keys,
      {
        id: "code-key",
        name: "Dedicated executor",
        protocol: "openai",
        base_url: "https://executor.example.com",
      },
    ]);
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await user.selectOptions(
      dialog.getByLabelText("Code service"),
      "workspace_daemon",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    expect(writes).toHaveLength(0);
    expect(
      within(dialog.getByLabelText("Executor API key")).queryByRole("option", {
        name: "Anthropic only",
      }),
    ).not.toBeInTheDocument();
    await user.selectOptions(
      dialog.getByLabelText("Executor API key"),
      "code-key",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0]).not.toHaveProperty("model");
    expect(writes[0].tool_services).toEqual({
      code_interpreter: {
        provider: "workspace_daemon",
        credential_id: "code-key",
      },
    });
  });

  it.each(["Name", "Instructions"])(
    "Code executor metadata-only edit of %s never serializes or drops saved services",
    async (label) => {
      const services = {
        code_interpreter: {
          provider: "workspace_daemon" as const,
          credential_id: "unavailable-code-key",
          owner_extension: { keep: true },
        },
        future_service: { keep: "unrecognized" },
      };
      const agent = { ...nativeAgent, tool_services: services };
      mount(agent);
      const { user, dialog } = await edit();
      await user.type(dialog.getByLabelText(label), " updated");
      await user.click(dialog.getByRole("button", { name: "Save agent" }));
      await waitFor(() => expect(writes).toHaveLength(1));
      expect(writes[0]).not.toHaveProperty("tool_services");
      expect({ ...agent, ...writes[0] }.tool_services).toEqual(services);
    },
  );

  it("Code executor unknown saved routing survives an image-only service edit", async () => {
    const services = {
      code_interpreter: {
        provider: "future_executor",
        credential_id: "future-code",
        contract: { version: 2 },
      },
      future_service: { keep: "unknown" },
    } as unknown as ToolServices;
    mount({ ...nativeAgent, tool_services: services });
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    expect(dialog.getByLabelText("Code service")).toHaveValue(
      "future_executor",
    );
    await user.selectOptions(
      dialog.getByLabelText("Image service"),
      "openai_images",
    );
    await user.selectOptions(
      dialog.getByLabelText("Image API key"),
      "image-key",
    );
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].tool_services?.code_interpreter).toEqual(
      services.code_interpreter,
    );
    expect(writes[0].tool_services?.future_service).toEqual(
      services.future_service,
    );
  });

  it("Code executor reset removes only its own override", async () => {
    const services: ToolServices = {
      code_interpreter: {
        provider: "workspace_daemon",
        credential_id: "llm-key",
      },
      image_generation: { provider: "platform" },
      web_search: { provider: "platform_deepseek" },
      future_service: { keep: true },
    };
    mount({ ...nativeAgent, tool_services: services });
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await user.selectOptions(dialog.getByLabelText("Code service"), "");
    await user.click(dialog.getByRole("button", { name: "Save agent" }));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(writes[0].tool_services).toEqual({
      image_generation: services.image_generation,
      web_search: services.web_search,
      future_service: services.future_service,
    });
  });

  it("Code executor controls disclose the key progressively in Chinese", async () => {
    mount();
    const { user, dialog } = await edit();
    await user.click(dialog.getByText("Tool services", { exact: true }));
    await i18n.changeLanguage("zh-CN");
    const service = await dialog.findByLabelText("代码执行服务");
    expect(dialog.queryByLabelText("执行器 API 密钥")).not.toBeInTheDocument();
    await user.selectOptions(service, "workspace_daemon");
    expect(dialog.getByLabelText("执行器 API 密钥")).toBeInTheDocument();
  });
});
