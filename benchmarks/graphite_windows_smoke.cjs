// Optional Windows ConPTY lane. Uses a separately installed node-pty, not a Kroma dependency.
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const assert = require('node:assert/strict');
const pty = require(process.env.KROMA_NODE_PTY_PATH || 'node-pty');
const binary = path.resolve(process.argv[2] || 'target/release/kroma.exe');
const artifacts = path.resolve('reviews/tui-graphite/conpty');
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'kroma-graphite-'));
fs.mkdirSync(artifacts, { recursive: true });
let agentCalls = 0;
let child;
let exited;
let raw = '';
const steps = [];
const plain = () => raw.replace(/\x1b\].*?(?:\x07|\x1b\x5c)/gs, '').replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
function checkpoint(name) {
  fs.writeFileSync(path.join(artifacts, `${name}.ansi`), raw);
  steps.push(name);
  console.log(`ConPTY: ${name}`);
}
async function until(predicate, description, timeout = 15000) {
  const end = Date.now() + timeout;
  while (!predicate()) {
    if (exited) throw new Error(`${description}: process exited ${JSON.stringify(exited)}`);
    if (Date.now() > end) throw new Error(`Timed out: ${description}`);
    await new Promise(resolve => setTimeout(resolve, 25));
  }
}
const server = http.createServer((req, res) => {
  if (req.method === 'GET') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ data: [{ id: 'graphite-mock', context_length: 500000 }] }));
    return;
  }
  let body = '';
  req.on('data', chunk => { body += chunk; });
  req.on('end', () => {
    try {
      const request = JSON.parse(body);
      const agent = Array.isArray(request.tools) && request.tools.length > 0;
      const call = agent ? ++agentCalls : 0;
      const delta = agent && call === 1
        ? { role: 'assistant', tool_calls: [{ index: 0, id: 'graphite-call', type: 'function', function: { name: 'shell', arguments: JSON.stringify({ command: 'echo GRAPHITE_TOOL_OK' }) } }] }
        : { role: 'assistant', content: agent ? 'GRAPHITE_DONE. Verified the actual tool output.' : 'Graphite verification' };
      const chunk = { id: `mock-${call}`, object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: agent && call === 1 ? 'tool_calls' : 'stop' }], usage: { prompt_tokens: 1200, completion_tokens: 40, prompt_tokens_details: { cached_tokens: 300 } } };
      res.writeHead(200, { 'Content-Type': 'text/event-stream' });
      res.end(`data: ${JSON.stringify(chunk)}\n\ndata: [DONE]\n\n`);
    } catch (error) {
      res.writeHead(400); res.end(String(error));
    }
  });
});
(async () => {
  try {
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const home = path.join(scratch, 'home');
    fs.mkdirSync(home);
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('KROMA_')));
    Object.assign(env, { HOME: home, USERPROFILE: home, KROMA_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`, KROMA_API_KEY: 'local-test-only', OPENAI_API_KEY: 'local-test-only', KROMA_MODEL: 'graphite-mock', COLORTERM: 'truecolor', TERM: 'xterm-256color' });
    child = pty.spawn(binary, [], { name: 'xterm-256color', cols: 80, rows: 24, cwd: scratch, env });
    child.onData(data => { raw += data; });
    child.onExit(event => { exited = event; });
    await until(() => plain().includes('Describe a task'), 'welcome');
    checkpoint('home-80x24');
    child.write('/language zh\r');
    await until(() => plain().includes('界面设置已保存'), 'Chinese language switch');
    child.write('未发送草稿');
    await until(() => plain().includes('发送草稿') && raw.includes('未'), 'Unicode draft');
    child.write('\x10');
    await until(() => plain().includes('命令索引'), 'command dock');
    checkpoint('picker-80x24');
    for (const [cols, rows] of [[40,16],[160,40]]) {
      const before = raw.length;
      child.resize(cols, rows);
      await until(() => raw.length > before, `resize ${cols}x${rows}`);
    }
    child.write('\x1b');
    await until(() => raw.length > 0 && !exited, 'close picker');
    assert.equal(agentCalls, 0, 'UI exploration must not call the model');
    child.write('\x03');
    child.write('Graphite verification\r');
    await until(() => plain().includes('GRAPHITE_DONE'), 'tool loop completion', 25000);
    checkpoint('conversation-160x40');
    const beforeInspect = agentCalls;
    child.write('/inspect \r');
    await until(() => plain().includes('执行记录'), 'execution picker');
    child.write('\r');
    await until(() => plain().includes('执行快照'), 'read-only inspector');
    checkpoint('inspector-160x40');
    child.write('\x1b');
    child.write('/usage \r');
    await until(() => plain().includes('缓存读取'), 'cache details in usage');
    checkpoint('usage-160x40');
    assert.equal(agentCalls, beforeInspect, 'inspection and usage must not call the model');
    assert.equal(agentCalls, 2, 'exactly one tool call and one final model response');
    child.write('\x11');
    const end = Date.now() + 6000;
    while (!exited && Date.now() < end) await new Promise(resolve => setTimeout(resolve, 25));
    assert.equal(exited?.exitCode, 0, 'Ctrl+Q exits cleanly');
    fs.writeFileSync(path.join(artifacts, 'result.json'), JSON.stringify({ passed: true, steps, agentCalls, exitCode: exited.exitCode }, null, 2));
    console.log(`ConPTY PASS: ${steps.length} captures, ${agentCalls} model requests, exit 0`);
  } catch (error) {
    fs.writeFileSync(path.join(artifacts, 'failure.ansi'), raw);
    console.error(error);
    process.exitCode = 1;
  } finally {
    if (child && !exited) {
      child.kill();
      const end = Date.now() + 5000;
      while (!exited && Date.now() < end) await new Promise(resolve => setTimeout(resolve, 25));
    }
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    try { fs.rmSync(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 }); }
    catch (error) { console.warn(`Scratch retained after cleanup failure: ${error.code}`); }
  }
})();
