// A dropdown menu: its trigger opens and closes it; Escape, a choice, or a
// press anywhere outside it closes it.
export function createMenu({ root, trigger, doc, onSelect = () => {} }) {
  let open = false;
  const show = () => {
    open = true;
    root.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
  };
  const hide = () => {
    open = false;
    root.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
  };
  trigger.addEventListener("click", () => (open ? hide() : show()));
  // A press outside the menu (and its trigger) closes it.
  root.addEventListener("pointerdown", (event) => {
    if (open && !root.contains(event.target) && !trigger.contains(event.target)) {
      hide();
    }
  });
  doc.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      hide();
    }
  });
  root.addEventListener("click", (event) => {
    const item = event.target.closest?.("[data-item]");
    if (item) {
      hide();
      onSelect(item.dataset.item);
    }
  });
  return { show, hide, isOpen: () => open };
}
