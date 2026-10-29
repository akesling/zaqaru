// Checks `views.js` under Node: node --test web/views.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { addressOf, colour, firstView, guessWidth, layerFor, pixelOf, shapeOf } from "./views.js";

const doom = { address: 0x3800_0000, pitch: 320, width: 320, height: 200, bytes: 1, format: "L8", clut: null };

test("bytes a layer scans out are its picture, pixel by pixel", () => {
  assert.equal(layerFor([doom], 0x3800_0000, 64000), doom);
  assert.equal(layerFor([doom], 0x3800_0000 + 64000, 16), null);
  const shape = shapeOf(doom, 0x3800_0000, 64000);
  assert.deepEqual(pixelOf(shape, 0x3800_0000 + 320 * 3 + 40), { x: 40, y: 3 });
  assert.equal(addressOf(shape, 40, 3), 0x3800_0000 + 1000);
  const wide = { ...doom, bytes: 2, pitch: 640, format: "RGB565" };
  assert.deepEqual(pixelOf(shapeOf(wide, wide.address, 128000), wide.address + 643), { x: 1, y: 1 });
});

test("bytes nothing scans out are laid out by a width their count suggests", () => {
  assert.equal(guessWidth(64000), 320);
  assert.equal(guessWidth(4096), 128);
  assert.equal(guessWidth(1000), 64);
  const shape = shapeOf(null, 0x2000_0000, 4096, 128);
  assert.deepEqual([shape.width, shape.height, shape.base], [128, 32, 0x2000_0000]);
});

test("colours as the format has them", () => {
  const clut = [0, 0, 0, 10, 20, 30];
  assert.deepEqual(colour("L8", [1], clut), [10, 20, 30]);
  assert.deepEqual(colour("L8", [7], null), [7, 7, 7]);
  assert.deepEqual(colour("RGB565", [0x00, 0xf8], null), [255, 0, 0]);
  assert.deepEqual(colour("ARGB8888", [1, 2, 3, 255], null), [3, 2, 1]);
  assert.equal(colour("L8", [null], clut), null);
});

test("text when it reads as text, a picture when it is one", () => {
  assert.equal(firstView([...Buffer.from("hello, world\n")], null), "text");
  assert.equal(firstView([0, 1, 2, 3], null), "hex");
  assert.equal(firstView([0, 1, 2, 3], doom), "picture");
  assert.equal(firstView(new Array(4096).fill(0xbf), null), "picture");
});
