import assert from "node:assert/strict";
import { test } from "node:test";
import { createMenu } from "../src/menu.js";

// Just enough of the DOM: nodes that hold children and dispatch events.
class FakeNode extends EventTarget {
  constructor(children = []) {
    super();
    this.children = children;
    this.hidden = true;
    this.attributes = {};
  }
  contains(node) {
    return node === this || this.children.some((child) => child.contains(node));
  }
  setAttribute(name, value) {
    this.attributes[name] = value;
  }
}

function setup() {
  const item = new FakeNode();
  const root = new FakeNode([item]);
  const trigger = new FakeNode();
  const canvas = new FakeNode();
  const doc = new FakeNode([root, trigger, canvas]);
  const menu = createMenu({ root, trigger, doc });
  return { menu, root, trigger, canvas, doc, item };
}

// A pointerdown on `target`, as it reaches the document after bubbling up.
function pressAt(doc, target) {
  const event = new Event("pointerdown");
  Object.defineProperty(event, "target", { value: target });
  doc.dispatchEvent(event);
}

test("the trigger opens and closes the menu", () => {
  const { menu, root, trigger } = setup();
  trigger.dispatchEvent(new Event("click"));
  assert.equal(menu.isOpen(), true);
  assert.equal(root.hidden, false);
  trigger.dispatchEvent(new Event("click"));
  assert.equal(menu.isOpen(), false);
  assert.equal(root.hidden, true);
});

test("a press inside the menu keeps it open", () => {
  const { menu, doc, item } = setup();
  menu.show();
  pressAt(doc, item);
  assert.equal(menu.isOpen(), true);
});

test("a press outside the menu closes it", () => {
  const { menu, doc, canvas } = setup();
  menu.show();
  pressAt(doc, canvas);
  assert.equal(menu.isOpen(), false);
});

test("Escape closes the menu", () => {
  const { menu, doc } = setup();
  menu.show();
  const event = new Event("keydown");
  event.key = "Escape";
  doc.dispatchEvent(event);
  assert.equal(menu.isOpen(), false);
});
