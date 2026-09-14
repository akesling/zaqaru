// Test harness only. The page and Worker use browser APIs exclusively.
import { createServer } from 'node:http';
import { readFile, writeFile, mkdtemp, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { resolve, extname, sep } from 'node:path';
import { tmpdir } from 'node:os';
import { once } from 'node:events';
const root = resolve('.');
const server = createServer(async (req, res) => {
  try {
    const path = resolve(root, '.' + new URL(req.url, 'http://localhost').pathname);
    if (!path.startsWith(root + sep)) throw new Error('outside root');
    const data = await readFile(path);
    res.setHeader('Content-Type', ({'.html':'text/html', '.mjs':'text/javascript', '.js':'text/javascript', '.wasm':'application/wasm'})[extname(path)] ?? 'application/octet-stream');
    res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const profile = await mkdtemp(resolve(tmpdir(), 'zaqaru-browser-'));
const chrome = spawn(process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ['--headless=new', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank'],
  {stdio: ['ignore', 'ignore', 'pipe']});
let socket;
try {
  const endpoint = await new Promise((resolve, reject) => {
    let stderr = '';
    chrome.stderr.on('data', data => { stderr += data; const match = stderr.match(/DevTools listening on (ws:\/\/\S+)/); if (match) resolve(match[1]); });
    chrome.once('error', reject);
    chrome.once('exit', code => reject(new Error(`Chrome exited: ${code}`)));
  });
  socket = new WebSocket(endpoint);
  await once(socket, 'open');
  let id = 0;
  const pending = new Map();
  socket.addEventListener('message', ({data}) => { const reply = JSON.parse(data); const p = pending.get(reply.id); if (p) { pending.delete(reply.id); reply.error ? p.reject(reply.error) : p.resolve(reply.result); } });
  const command = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const key = ++id; pending.set(key, {resolve, reject}); socket.send(JSON.stringify({id:key, method, params, sessionId}));
  });
  const {targetId} = await command('Target.createTarget', {url:'about:blank'});
  const {sessionId} = await command('Target.attachToTarget', {targetId, flatten:true});
  await command('Page.enable', {}, sessionId);
  // Attach before navigating, then wait for the new document. Evaluating in
  // the initial about:blank context races its destruction during navigation.
  const loaded = new Promise(resolve => {
    const listener = ({data}) => {
      const event = JSON.parse(data);
      if (event.sessionId === sessionId && event.method === 'Page.loadEventFired') {
        socket.removeEventListener('message', listener);
        resolve();
      }
    };
    socket.addEventListener('message', listener);
  });
  const query = process.argv[2] === undefined ? '' : `?regionMembers=${encodeURIComponent(process.argv[2])}`;
  const navigation = await command('Page.navigate', {url:`http://127.0.0.1:${server.address().port}/tools/evolution/index.html${query}`}, sessionId);
  if (navigation.errorText) throw new Error(navigation.errorText);
  await loaded;
  const result = await command('Runtime.evaluate', {
    expression: 'new Promise((resolve, reject) => { const started = Date.now(); const timer = setInterval(() => { if (window.experimentResult) { clearInterval(timer); resolve(window.experimentResult); } else if (Date.now() - started > 180000) { clearInterval(timer); reject(new Error("experiment timeout")); } }, 100); })',
    awaitPromise:true, returnByValue:true,
  }, sessionId);
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
  const value = result.result.value;
  console.log(JSON.stringify(value, null, 2));
  await writeFile('benchmark-results/evolution-browser.json', JSON.stringify(value, null, 2));
  if (value.result) await writeFile(`benchmark-results/evolution-browser-${value.result.stdout.split(' ')[0]}.json`, JSON.stringify(value.result, null, 2));
  if (value.result) await writeFile(`benchmark-results/evolution-browser-${value.result.stdout.split(' ')[0]}-${value.result.totalRetired}.json`, JSON.stringify(value.result, null, 2));
  if (value.error) throw new Error(value.error);
} finally {
  socket?.close();
  chrome.kill();
  await once(chrome, 'exit').catch(() => {});
  server.close();
  await rm(profile, {recursive:true, force:true});
}
