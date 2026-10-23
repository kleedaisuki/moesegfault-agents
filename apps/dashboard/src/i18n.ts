import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en.json";
import zh from "./locales/zh-CN.json";
function preferredLanguage() {
  try {
    const stored = localStorage.getItem("magents.language");
    if (stored === "en" || stored === "zh-CN") return stored;
  } catch {
    /* Preference storage is optional. */
  }
  return navigator.language?.toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}
i18n.use(initReactI18next).init({
  resources: { en: { translation: en }, "zh-CN": { translation: zh } },
  lng: preferredLanguage(),
  fallbackLng: "en",
  keySeparator: false,
  nsSeparator: false,
  interpolation: { escapeValue: false },
  initAsync: false,
});
i18n.on("languageChanged", (language) => {
  document.documentElement.lang = language;
  document.title =
    language === "zh-CN" ? "智能体 · moeSegFault" : "Agents · moeSegFault";
});
document.documentElement.lang = i18n.resolvedLanguage ?? "en";
export default i18n;
export const locale = () =>
  i18n.resolvedLanguage === "zh-CN" ? "zh-CN" : "en-US";
