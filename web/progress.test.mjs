// Checks `progress.js` under Node: node --test web/progress.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { boardTime, progress, progressHtml, wait } from "./progress.js";

test("times read as the page writes them", () => {
  assert.equal(boardTime(44.5054e12), "44.505 s");
  assert.equal(boardTime(2.5e9), "2.500 ms");
  assert.equal(wait(800), "a moment");
  assert.equal(wait(42_000), "42 s");
  assert.equal(wait(180_000), "3 min");
});

test("a run to the window, then following in it", () => {
  const run = { start: 10e12, from: 44e12, to: 45e12, elapsed: 10_000 };
  const before = progress({ ...run, board: 27.5e12 });
  assert.equal(before.fraction, 0.5);
  assert.match(before.phase, /^running the board again from its checkpoint at 10\.000 s: at 27\.500 s of 45\.000 s$/);
  assert.equal(before.left, "about 10 s left");
  const within = progress({ ...run, board: 44.5e12 });
  assert.match(within.phase, /^following from 44\.000 s: at 44\.500 s/);
});

test("nothing said about what is left until there is enough done to say it", () => {
  assert.equal(progress({ start: 0, from: null, to: 100, board: 1, elapsed: 5000 }).left, null);
  assert.equal(progress({ start: 0, from: null, to: 100, board: 50, elapsed: 100 }).left, null);
  assert.match(progressHtml(null), /restoring a checkpoint/);
  assert.match(progressHtml({ start: 0, from: null, to: 100, board: 25, elapsed: 3000 }), /width:25\.0%.*about 9 s left/);
});

test("following, what is left is said at following's pace", () => {
  // Ran to 31.959 s fast, then followed 0.6 s of the board in 60 s.
  const report = { start: 0, from: 31.959e12, to: 32.959e12, board: 32.559e12, elapsed: 62_000, followed: { board: 31.959e12, elapsed: 2_000 } };
  assert.equal(progress(report).left, "about 40 s left");
  assert.equal(progress({ ...report, board: 31.96e12, elapsed: 2_200 }).left, null, "not until it has gone a while");
});
