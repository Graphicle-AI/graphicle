"use strict";
// The colour theme, applied before the first paint: pages load this file in
// <head> before their stylesheets, without defer (the Content-Security-Policy
// allows no inline script). The choice is "system" (the default: follow the operating system),
// "light" or "dark"; it is kept per browser in localStorage under
// "graphicle-theme", as the graph export page keeps its own, and when storage
// is unavailable it lasts until the page is closed. The theme is set as
// data-theme on <html> (style.css); buttons with data-theme-choice pick it.
(() => {
  const KEY = "graphicle-theme";
  const CHOICES = ["system", "light", "dark"];
  // The browser bar's colour for each theme: the page background (--ink).
  const BAR = {light: "#f7f3ea", dark: "#0a0a12"};
  const light = window.matchMedia ? window.matchMedia("(prefers-color-scheme: light)") : null;
  let unsaved = null;
  function stored() {
    try {
      const value = window.localStorage.getItem(KEY);
      return CHOICES.includes(value) ? value : null;
    } catch {
      return null;
    }
  }
  function choice() { return unsaved || stored() || "system"; }
  function resolve(chosen) { return chosen === "light" || chosen === "dark" ? chosen : (light && light.matches ? "light" : "dark"); }
  function apply() {
    const chosen = choice();
    const theme = resolve(chosen);
    document.documentElement.dataset.theme = theme;
    // With "system" each meta keeps the colour of the scheme its media names;
    // a picked theme sets both.
    for (const meta of document.querySelectorAll('meta[name="theme-color"]')) {
      const scheme = (meta.getAttribute("media") || "").includes("light") ? "light" : "dark";
      meta.content = BAR[chosen === "system" ? scheme : theme];
    }
    for (const button of document.querySelectorAll("[data-theme-choice]")) {
      const selected = button.dataset.themeChoice === chosen;
      button.setAttribute("aria-pressed", String(selected));
      button.classList.toggle("selected", selected);
    }
  }
  function pick(value) {
    if (!CHOICES.includes(value)) return;
    unsaved = null;
    try {
      if (value === "system") window.localStorage.removeItem(KEY);
      else window.localStorage.setItem(KEY, value);
    } catch {
      unsaved = value;
    }
    apply();
  }
  apply();
  if (light) {
    const follow = () => { if (choice() === "system") apply(); };
    if (light.addEventListener) light.addEventListener("change", follow);
    else if (light.addListener) light.addListener(follow);
  }
  // Another tab of this app changed the choice.
  window.addEventListener("storage", event => { if (event.key === KEY || event.key === null) apply(); });
  document.addEventListener("click", event => {
    const button = event.target instanceof Element ? event.target.closest("[data-theme-choice]") : null;
    if (button) pick(button.dataset.themeChoice);
  });
  document.addEventListener("DOMContentLoaded", apply);
})();
