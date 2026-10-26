// Checks the Hubris bench module under Node, through the same host the page
// uses: it boots, a session on port 80 greets, and Humility, attached to
// the board through the kernel's probe, lists Hubris's tasks.
//
//   node demo/hubris/check.mjs <hubris.wasm> [line...]
//
// The lines default to `board` and `humility tasks`, typed one after another
// into one session, as the page's terminal does; `wait:N` runs N seconds
// without typing, and `upload:PATH` sends a file of this host's as the page
// does one dropped on its terminal; `gdb:COMMAND` sends `monitor COMMAND`
// to the board's GDB server, as GDB would — a `qRcmd` packet on the
// probe's port — and shows what it printed. Prints what each line answered
// and how long it took, in wall time and in instructions, and exits 0 when
// `humility tasks` showed a task RUNNING.

import { readFileSync } from "node:fs";
import { basename } from "node:path";
import { Container, Edge, KIND, standardMounts, text } from "../../web/zaqaru.js";

const [modulePath, ...asked] = process.argv.slice(2);
if (!modulePath) {
  console.error("usage: node demo/hubris/check.mjs <hubris.wasm> [line...]");
  process.exit(2);
}
const lines = asked.length ? asked : ["board", "humility tasks"];
const PORT = 80;
const GDB = 3333;
const PROMPT = "\n$ ";
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const started = performance.now();
const module = await WebAssembly.compile(readFileSync(modulePath));
const edge = new Edge([PORT, GDB]);
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

// The board's GDB server, opened at the first `gdb:` line.
let gdb = null;
let gdbScreen = "";
const packet = (payload) => {
  const sum = [...payload].reduce((a, c) => (a + c.charCodeAt(0)) & 0xff, 0);
  return `$${payload}#${sum.toString(16).padStart(2, "0")}`;
};
const hex = (s) => [...new TextEncoder().encode(s)].map((b) => b.toString(16).padStart(2, "0")).join("");
const unhex = (h) => new TextDecoder().decode(new Uint8Array((h.match(/../g) ?? []).map((b) => parseInt(b, 16))));

let retired = 0;
// Runs the container until `done()` or `limit` ms pass; when it is idle, it
// waits as long as the container said it could be left, as the page does.
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
    if (turn.kind === KIND.IDLE) await (turn.wait >= 1 ? sleep(Math.min(turn.wait, 50)) : new Promise(setImmediate));
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
  const monitor = /^gdb:(.+)$/.exec(line);
  if (monitor) {
    gdb ??= edge.open(GDB, { received: (data) => (gdbScreen += text(data)), ended: () => {} });
    const from = gdbScreen.length;
    gdb.send(new TextEncoder().encode(packet(`qRcmd,${hex(monitor[1])}`)));
    // Console output comes as `O` packets, then `OK` ends the command; an
    // empty reply is a command the board does not know.
    const done = () => /\$(OK|E\d\d|)#/.test(gdbScreen.slice(from));
    const answered = await runUntil(done, 60000);
    const said = [...gdbScreen.slice(from).matchAll(/\$O([0-9a-f]+)#/g)].map((m) => unhex(m[1])).join("");
    console.log(`(gdb) monitor ${monitor[1]}\n${said}`);
    const unknown = /\$#00/.test(gdbScreen.slice(from)) ? " (not a command the board knows)" : "";
    console.error(`check: ${line}${unknown}${answered ? "" : ` (no answer: ${JSON.stringify(gdbScreen.slice(from))})`}`);
    continue;
  }
  const mark = screen.length;
  const uploading = /^upload:(.+)$/.exec(line);
  if (uploading) {
    const bytes = readFileSync(uploading[1]);
    const head = new TextEncoder().encode(`upload ${basename(uploading[1])} ${bytes.length}\n`);
    session.send(new Uint8Array([...head, ...bytes]));
  } else {
    session.send(new TextEncoder().encode(`${line}\n`));
  }
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
