(() => {
  const root = document.documentElement;
  let theme = 'system';
  let language = navigator.language?.toLowerCase().startsWith('zh') ? 'zh-CN' : 'en';
  try {
    const storedTheme = localStorage.getItem('magents.theme');
    const storedLanguage = localStorage.getItem('magents.language');
    if (['light', 'dark', 'system'].includes(storedTheme)) theme = storedTheme;
    if (['en', 'zh-CN'].includes(storedLanguage)) language = storedLanguage;
  } catch { /* Browser privacy settings may deny preference storage. */ }
  root.lang = language;
  root.dataset.themePreference = theme;
  const media = matchMedia('(prefers-color-scheme: dark)');
  const apply = () => {
    root.dataset.theme = root.dataset.themePreference === 'system' ? media.matches ? 'dark' : 'light' : root.dataset.themePreference;
    root.style.colorScheme = root.dataset.theme;
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', root.dataset.theme === 'dark' ? '#151c18' : '#f8f9f6');
  };
  apply();
  media.addEventListener('change', apply);
})();
