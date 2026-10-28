import { loadStrings, t } from "./i18n.js";
import { createMenu } from "./menu.js";

function menuItem(id, label) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "menu-item";
  button.dataset.item = id;
  button.setAttribute("role", "menuitem");
  button.textContent = label;
  return button;
}

function renderMenu(root) {
  root.replaceChildren(
    menuItem("open", t("menu.open")),
    menuItem("save", t("menu.saveTo")),
    menuItem("export", t("menu.exportImage")),
    menuItem("reset", t("menu.reset")),
  );
}

async function main() {
  await loadStrings("locales/en.json");
  document.getElementById("title").textContent = t("app.title");
  const root = document.getElementById("menu");
  renderMenu(root);
  createMenu({
    root,
    trigger: document.getElementById("menu-trigger"),
    doc: document,
    onSelect: (id) => {
      if (id === "reset") {
        document.getElementById("canvas").replaceChildren();
      }
    },
  });
}

main();
