// Declaration extents of JS/TS sources from the TypeScript parser, for the
// call-graph oracle (oracle_eval.py). SCIP gives enclosing ranges for
// function and class declarations only; calls also sit in function-valued
// variables and class properties (`const f = () => {}`, `handle = () => {}`)
// and in other initializers (`const action = register({ perform() {...} })`).
//
// Output: {file: [[name, name_line, start_line, start_char, end_line,
// end_char, kind], ...]} (0-based lines), kind "function", "class" or
// "value" (other initializers).
// Usage: node ts_extents.js ROOT FILE... > extents.json
// Needs the `typescript` package (TS_MODULE may point at it).
"use strict";
const fs = require("fs");
const path = require("path");
const ts = require(process.env.TS_MODULE || "typescript");

const [root, ...files] = process.argv.slice(2);
const out = {};
for (const rel of files) {
  const text = fs.readFileSync(path.join(root, rel), "utf8");
  const kind = rel.endsWith(".tsx") ? ts.ScriptKind.TSX
    : rel.endsWith(".ts") || rel.endsWith(".mts") || rel.endsWith(".cts") ? ts.ScriptKind.TS
    : ts.ScriptKind.JSX;
  const sf = ts.createSourceFile(rel, text, ts.ScriptTarget.Latest, true, kind);
  const extents = [];
  const pos = (p) => sf.getLineAndCharacterOfPosition(p);
  const add = (nameNode, body, what) => {
    if (!nameNode || !body || !ts.isIdentifier(nameNode) && !ts.isPrivateIdentifier(nameNode)
        && !ts.isStringLiteral(nameNode)) return;
    const s = pos(body.getStart(sf));
    const e = pos(body.getEnd());
    const n = pos(nameNode.getStart(sf));
    extents.push([nameNode.text, n.line, s.line, s.character, e.line, e.character, what]);
  };
  const isFn = (n) => !!n && (ts.isArrowFunction(n) || ts.isFunctionExpression(n));
  const visit = (node) => {
    if (ts.isFunctionDeclaration(node) || ts.isMethodDeclaration(node)
        || ts.isGetAccessorDeclaration(node) || ts.isSetAccessorDeclaration(node)) {
      add(node.name, node, "function");
    } else if (ts.isClassDeclaration(node)) {
      add(node.name, node, "class");
    } else if ((ts.isVariableDeclaration(node) || ts.isPropertyDeclaration(node)
        || ts.isPropertyAssignment(node)) && node.initializer) {
      add(node.name, node.initializer, isFn(node.initializer) ? "function" : "value");
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  out[rel] = extents;
}
process.stdout.write(JSON.stringify(out));
