// Compare shipped UI calls with Rust command arguments. No app or NAS access.
const fs = require('node:fs');
const path = require('node:path');
const ts = require('typescript');
const root = path.resolve(__dirname, '..');
const rust = fs.readFileSync(path.join(root, 'src-tauri/src/commands/mod.rs'), 'utf8');
const camel = name => name.replace(/^_+/, '').replace(/_([a-z])/g, (_, c) => c.toUpperCase());
const commands = new Map();
for (const match of rust.matchAll(/#\[tauri::command[^\]]*\]\s*pub (?:async )?fn (\w+)\(([^)]*)\)/g)) {
  const args = [...match[2].matchAll(/(\w+):\s*([\w:]+)(?:<[^;]*?>)?/g)]
    .filter(([, , type]) => !['State', 'tauri::AppHandle'].includes(type));
  commands.set(match[1], args.filter(([, , type]) => type !== 'Option').map(([, name]) => camel(name)));
}
let calls = 0;
const errors = [];
for (const filename of fs.readdirSync(path.join(root, 'official-ui/assets')).filter(f => f.endsWith('.js'))) {
  const source = ts.createSourceFile(filename, fs.readFileSync(path.join(root, 'official-ui/assets', filename), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  function visit(node) {
    if (ts.isCallExpression(node) && /^invoke(?:\$\d+)?$/.test(node.expression.getText(source)) && node.arguments[0] && ts.isStringLiteral(node.arguments[0])) {
      const name = node.arguments[0].text;
      if (commands.has(name)) {
        calls++;
        const object = node.arguments[1];
        if (object && !ts.isObjectLiteralExpression(object)) { errors.push(`${name}: dynamic payload requires review`); }
        else {
          const fields = new Set(object ? object.properties.filter(p => p.name).map(p => p.name.getText(source).replace(/^["']|["']$/g, '')) : []);
          for (const required of commands.get(name)) if (!fields.has(required)) errors.push(`${name}: missing ${required}; sent ${[...fields].join(', ')}`);
        }
      } else if (!name.includes(':')) errors.push(`${name}: no Rust application command`);
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
}
for (const error of [...new Set(errors)]) console.error(error);
console.log(`Checked ${calls} UI calls against ${commands.size} application commands.`);
if (errors.length) process.exitCode = 1;
