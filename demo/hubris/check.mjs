// Checks the Hubris bench module under Node, through the same host the page
// uses: it boots, a session on port 80 greets, and Humility, attached to
// the board through the kernel's probe, lists Hubris's tasks.
//
//   node demo/hubris/check.mjs <hubris.wasm> [line...]
//
// The lines default to `board` and `humility tasks`, typed one after another
// into one session, as the page's terminal does; `wait:N` runs N seconds
// without typing. Prints what each line answered and how long it took, in
// wall time and in instructions, and exits 0 when `humility tasks` showed
// a task RUNNING.

import { readFileSync } from "node:fs";
import { Container, Edge, KIND, standardMounts, text } from "../../web/zaqaru.js";

const [modulePath, ...asked] = process.argv.slice(2);
if (!modulePath) {
  console.error("usage: node demo/hubris/check.mjs <hubris.wasm> [line...]");
  process.exit(2);
}
const lines = asked.length ? asked : ["board", "humility tasks"];
const PORT = 80;
const PROMPT = "\n$ ";
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const started = performance.now();
const module = await WebAssembly.compile(readFileSync(modulePath));
const edge = new Edge([PORT]);
const console_ = [];
const mounts = standardMounts({ seed: null, edge, echo: (path, data) => console_.push(text(data)) });
const container = await Container.instantiate(module, mounts);
container.step(0);

let screen = "";
let ended = false;
const session = edge.open(PORT, {
  received: (data) => (screen += text(data)),
  ended: () => (ended = true),
});

let retired = 0;
// Runs the container until `done()` or `limit` ms pass; it sleeps when the
// container is idle, as the page does.
async function runUntil(done, limit) {
  const from = performance.now();
  while (!done() && performance.now() - from < limit) {
    const turn = container.step(retired + 4000000);
    retired = container.value("statistics").retired;
    if (turn.kind === KIND.FINISHED) {
      console.error(`check: the container exited ${turn.status}`);
      console.error(console_.join(""));
      process.exit(1);
    }
    if (turn.kind === KIND.IDLE) await sleep(5);
  }
  return done();
}

if (!(await runUntil(() => screen.endsWith("$ "), 60000))) {
  console.error(`check: no prompt on port 80 after 60 s${ended ? " (the session ended)" : ""}: ${JSON.stringify(screen)}`);
  process.exit(1);
}
console.error(`check: greeted in ${((performance.now() - started) / 1000).toFixed(1)} s, ${retired.toLocaleString()} instructions`);

let ok = false;
for (const line of lines) {
  const waiting = /^wait:(\d+(?:\.\d+)?)$/.exec(line);
  if (waiting) {
    const fromRetired = retired;
    const seconds = Number(waiting[1]);
    await runUntil(() => false, seconds * 1000);
    console.error(`check: waited ${seconds} s, ${((retired - fromRetired) / 1e6).toFixed(0)} M instructions`);
    continue;
  }
  const mark = screen.length;
  session.send(new TextEncoder().encode(`${line}\n`));
  const from = performance.now();
  const fromRetired = retired;
  const answered = await runUntil(() => screen.length > mark && screen.endsWith(PROMPT), 600000);
  const answer = screen.slice(mark, answered ? -PROMPT.length : undefined);
  const seconds = (performance.now() - from) / 1000;
  const instructions = retired - fromRetired;
  console.log(`$ ${line}\n${answer}`);
  console.error(`check: ${line}: ${seconds.toFixed(1)} s, ${(instructions / 1e6).toFixed(0)} M instructions${answered ? "" : " (no prompt after it)"}`);
  if (line === "humility tasks" && answer.includes("RUNNING")) ok = true;
}
process.exit(ok || !lines.includes("humility tasks") ? 0 : 1);
