// Run the shipped invoke wrapper against a non-writable Tauri bridge.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('typescript');
const root = path.resolve(__dirname, '..');
const asset = fs.readdirSync(path.join(root, 'official-ui/assets')).find(f => /^index-.*\.js$/.test(f));
const source = ts.createSourceFile(asset, fs.readFileSync(path.join(root, 'official-ui/assets', asset), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
let wrapper;
function visit(node) {
  if (ts.isFunctionDeclaration(node) && node.name?.text === 'invoke') wrapper = node.getText(source);
  ts.forEachChild(node, visit);
}
visit(source);
assert.ok(wrapper, 'shipped invoke wrapper missing');
const calls = [];
const window = {};
Object.defineProperty(window, '__TAURI_INTERNALS__', {value: Object.freeze({invoke: async (cmd, args) => {
  calls.push({cmd, args});
  if (cmd === 'reject_me') throw new Error('test failure');
  return 'ok';
}})});
const context = {window};
vm.createContext(context);
vm.runInContext(wrapper, context);
(async () => {
  const args = {serviceTypes: ['_fnos._tcp.local.']};
  assert.equal(await context.invoke('plugin:mdns|browse_many', args), 'ok');
  assert.equal(calls[0].cmd, 'mdns_browse_many');
  assert.equal(calls[0].args, args);
  await context.invoke('plugin:mdns|stop_browse', {});
  assert.equal(calls[1].cmd, 'mdns_stop_browse');
  await context.invoke('plugin:window|show', {});
  assert.equal(calls[2].cmd, 'plugin:window|show');
  await assert.rejects(context.invoke('reject_me', {password: 'do-not-log'}), /test failure/);
  const log = calls.find(c => c.cmd === 'js_log');
  assert.ok(log.args.message.includes('reject_me'));
  assert.ok(!JSON.stringify(log.args).includes('do-not-log'));
  console.log('PASS: mDNS routing, immutable Tauri bridge, plugin passthrough, error logging');
})().catch(error => { console.error(error); process.exitCode = 1; });
