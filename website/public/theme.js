(function () {
  function getTheme() {
    return document.documentElement.getAttribute('data-theme');
  }

  function setTheme(theme) {
    document.documentElement.setAttribute('data-theme', theme);
    localStorage.setItem('markrust-theme', theme);
  }

  function toggleTheme() {
    const current = getTheme();
    const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
    const isDark = current === 'dark' || (!current && prefersDark);
    setTheme(isDark ? 'light' : 'dark');
  }

  document.querySelectorAll('.theme-toggle').forEach((btn) => {
    btn.addEventListener('click', toggleTheme);
  });
})();
