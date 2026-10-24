import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import i18n from "./i18n";
import { UsagePage } from "./UsagePage";
import type { Account, Usage } from "./api";
const usage: Usage = {
  wallet: {
    budget_microusd: "0",
    spent_microusd: "0",
    reserved_microusd: "0",
    available_microusd: "0",
  },
  by_resource: [],
};
function mount(subscription_url?: string) {
  const client = new QueryClient({
    defaultOptions: { queries: { staleTime: Infinity, retry: false } },
  });
  client.setQueryData(["usage"], usage);
  const account: Account = {
    account_id: "staging-account",
    plan: "free",
    billing_mode: "test",
    subscription_url,
  };
  return render(
    <QueryClientProvider client={client}>
      <UsagePage account={account} />
    </QueryClientProvider>,
  );
}
afterEach(() => {
  cleanup();
  i18n.changeLanguage("en");
  window.history.replaceState(null, "", "/");
});
for (const language of ["en", "zh-CN"])
  describe(`subscription environment boundary (${language})`, () => {
    it.each([undefined, ""])(
      "hides subscription navigation when backend URL is %s",
      async (url) => {
        await i18n.changeLanguage(language);
        mount(url);
        expect(
          screen.queryByRole("link", { name: i18n.t("Manage subscription") }),
        ).not.toBeInTheDocument();
        expect(
          document.querySelector(
            'a[href^="https://subscribe.moesegfault.dev"]',
          ),
        ).toBeNull();
      },
    );
    it("uses the backend-provided staging URL without browser origin inference", async () => {
      await i18n.changeLanguage(language);
      mount("https://subscribe-staging.moesegfault.dev/");
      expect(
        screen.getByRole("link", { name: i18n.t("Manage subscription") }),
      ).toHaveAttribute("href", "https://subscribe-staging.moesegfault.dev/");
      expect(
        screen.getByRole("link", { name: i18n.t("Manage subscription") }),
      ).not.toHaveAttribute("target");
      expect(
        document.querySelector('a[href^="https://subscribe.moesegfault.dev"]'),
      ).toBeNull();
    });
    it("offers a fixed retry after a failed authenticated subscription refresh", async () => {
      await i18n.changeLanguage(language);
      window.history.replaceState(null, "", "/?billing_refresh=failed#usage");
      mount();
      expect(screen.getByRole("alert")).toHaveTextContent(
        i18n.t(
          "Your subscription could not be refreshed. Existing access has not been removed.",
        ),
      );
      expect(
        screen.getByRole("link", {
          name: i18n.t("Retry subscription refresh"),
        }),
      ).toHaveAttribute("href", "/billing/return");
    });
  });
