import { useState } from "react";
import { useTranslation } from "react-i18next";
export type Theme = "system" | "light" | "dark";
export function setTheme(theme: Theme) {
  const root = document.documentElement;
  root.dataset.themePreference = theme;
  root.dataset.theme =
    theme === "system"
      ? window.matchMedia?.("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light"
      : theme;
  root.style.colorScheme = root.dataset.theme;
  try {
    localStorage.setItem("magents.theme", theme);
  } catch {
    /* Preferences remain usable without storage. */
  }
  document
    .querySelector('meta[name="theme-color"]')
    ?.setAttribute(
      "content",
      root.dataset.theme === "dark" ? "#151c18" : "#f8f9f6",
    );
}
export function Preferences() {
  const { t, i18n } = useTranslation();
  const [theme, updateTheme] = useState<Theme>(
    (document.documentElement.dataset.themePreference as Theme) || "system",
  );
  return (
    <div className="preferences">
      <select
        aria-label={t("Language")}
        value={i18n.resolvedLanguage}
        onChange={(e) => {
          const language = e.target.value;
          i18n.changeLanguage(language);
          try {
            localStorage.setItem("magents.language", language);
          } catch {
            /* Preference storage is optional. */
          }
        }}
      >
        <option value="en">English</option>
        <option value="zh-CN">简体中文</option>
      </select>
      <select
        aria-label={t("Appearance")}
        value={theme}
        onChange={(e) => {
          const preference = e.target.value as Theme;
          updateTheme(preference);
          setTheme(preference);
        }}
      >
        <option value="system">{t("System")}</option>
        <option value="light">{t("Light")}</option>
        <option value="dark">{t("Dark")}</option>
      </select>
    </div>
  );
}
