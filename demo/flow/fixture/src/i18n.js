// The app's texts, by dotted key ("menu.reset"), from a locale file.
let strings = {};

export async function loadStrings(url) {
  strings = await (await fetch(url)).json();
}

export function setStrings(value) {
  strings = value;
}

export function t(key) {
  const found = key
    .split(".")
    .reduce((node, part) => (node && typeof node === "object" ? node[part] : undefined), strings);
  return typeof found === "string" ? found : key;
}
