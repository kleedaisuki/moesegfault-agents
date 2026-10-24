import { afterEach, describe, expect, it } from "vitest";
import i18n from "./i18n";
import en from "./locales/en.json";
import zh from "./locales/zh-CN.json";
import {
  ApiError,
  localizedError,
  microDollars,
  formattedDate,
  usageTotals,
} from "./api";
import { sessionPollInterval } from "./polling";
afterEach(() => {
  i18n.changeLanguage("en");
  localStorage.clear();
});
describe("complete localized UI", () => {
  it("has matching nonempty resources for every literal UI key", () => {
    expect(Object.keys(zh).sort()).toEqual(Object.keys(en).sort());
    const modules = import.meta.glob(
      "./{App,PublicEntry,Agents,Access,Conversation,UsagePage,Preferences,Skills,ui}.tsx",
      { query: "?raw", import: "default", eager: true },
    );
    for (const source of Object.values(modules))
      for (const match of (source as string).matchAll(/\bt\(\s*"([^"]+)"/g)) {
        expect(en, match[1]).toHaveProperty(match[1]);
        expect((zh as Record<string, string>)[match[1]]).toBeTruthy();
      }
  });
  it("localizes failures without leaking raw English fallback in Chinese", () => {
    i18n.changeLanguage("zh-CN");
    expect(localizedError(new ApiError("upstream private error", 429))).toBe(
      "已达到套餐或请求频率限制。",
    );
    expect(
      localizedError(
        new Error("Choose a registered model with available context metadata."),
      ),
    ).toBe(zh["Choose a registered model with available context metadata."]);
    expect(localizedError(new TypeError("Failed to fetch"))).toBe(
      zh["Network request failed"],
    );
  });
  it("formats exact microUSD above the JavaScript safe integer limit", () => {
    expect(microDollars("9007199254740993")).toBe("$9,007,199,254.740993");
    expect(microDollars("1")).toBe("$0.000001");
    expect(microDollars("-1")).toBe("-$0.000001");
    expect(microDollars("0")).toBe("$0.00");
    expect(microDollars("invalid")).toBe("—");
    i18n.changeLanguage("zh-CN");
    expect(microDollars("1234500")).toContain("1.2345");
    expect(formattedDate(1792764000)).toMatch(/2026年10月/);
  });
  it("sums resource charges without floating point or treating pending as settled", () => {
    expect(
      usageTotals({
        wallet: {
          budget_microusd: "20",
          spent_microusd: "1",
          reserved_microusd: "2",
          available_microusd: "17",
        },
        by_resource: [
          { resource: "model", cost_microusd: "9007199254740993" },
          { resource: "r2_operation", cost_microusd: "3" },
          { resource: "durable_object", cost_microusd: "2" },
        ],
      }),
    ).toEqual({
      total: 9007199254740998n,
      model: 9007199254740993n,
      container: 0n,
      storage: 3n,
    });
  });
});
describe("session request policy", () => {
  it("polls active tasks quickly, backs off idle sessions and never polls archived sessions", () => {
    const session = { id: "s", agent_id: "a", last_input_at_ms: 1000 };
    expect(
      sessionPollInterval({ ...session, status: "running" }, null, 0, 100000),
    ).toBe(2000);
    expect(
      sessionPollInterval({ ...session, status: "idle" }, null, 0, 2000),
    ).toBe(5000);
    expect(
      sessionPollInterval({ ...session, status: "idle" }, null, 0, 30000),
    ).toBe(15000);
    expect(
      sessionPollInterval({ ...session, status: "idle" }, null, 0, 100000),
    ).toBe(30000);
    expect(
      sessionPollInterval({ ...session, status: "closed" }, null, 0, 100000),
    ).toBe(false);
  });
  it("stops forbidden/missing polling and bounds transient error backoff", () => {
    expect(
      sessionPollInterval(undefined, new ApiError("expired", 401), 1),
    ).toBe(false);
    expect(
      sessionPollInterval(undefined, new ApiError("missing", 404), 1),
    ).toBe(false);
    expect(
      sessionPollInterval(undefined, new ApiError("unavailable", 503), 1),
    ).toBe(4000);
    expect(
      sessionPollInterval(undefined, new ApiError("unavailable", 503), 50),
    ).toBe(30000);
  });
});
