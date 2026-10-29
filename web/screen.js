// The panel on the board's LTDC, drawn, and a keyboard for the board.
//
// What it reads, from the board's store: `board/display/{since}`, which
// answers the frame the panel shows only when it has been shown one since
// the count asked with, so a page that asks as often as it can draws each
// frame once and otherwise costs a few bytes. A frame comes as palette
// indices and the palette, when one L8 layer covers the panel — Doom's
// case, a byte a pixel — or as RGB.
//
// Live, the frames come without asking: the worker sends each as a slice
// shows it (see `pushed`), and the asking above is only for an instant
// stood at, or a board the worker has not sent one for lately.
//
// What it writes: keys. While the screen has the focus, each key going
// down or up is its scan code, set 1, as an AT keyboard sends it — the make
// code, the break code with the top bit set, and 0xe0 before the keys the
// AT added — sent as a `keys` line on a session of the bench's front door,
// which hands it to the board's keyboard USART. So what is typed is input
// the container records like any other, and the board replays it.
//
// And the mouse, on the same line, once a click on the screen has locked
// the pointer to it: a packet of three bytes, 0xf0 with the buttons held
// in its low bits (left, right, middle), then how far it moved across and
// up, each a signed byte (see the Doom task's port). Scan codes never
// begin 0xf0 to 0xf7, so the board tells the two apart. Movement is sent
// at most every `MOUSE_MS`, what a packet could not carry kept for the
// next, so a fast flick is not three bytes a pixel down a 115200-baud line.

/// KeyboardEvent.code to set 1 scan codes, extended ones as two bytes.
const SCAN_CODES = {
  Escape: [0x01], Digit1: [0x02], Digit2: [0x03], Digit3: [0x04], Digit4: [0x05], Digit5: [0x06],
  Digit6: [0x07], Digit7: [0x08], Digit8: [0x09], Digit9: [0x0a], Digit0: [0x0b], Minus: [0x0c],
  Equal: [0x0d], Backspace: [0x0e], Tab: [0x0f], KeyQ: [0x10], KeyW: [0x11], KeyE: [0x12],
  KeyR: [0x13], KeyT: [0x14], KeyY: [0x15], KeyU: [0x16], KeyI: [0x17], KeyO: [0x18], KeyP: [0x19],
  BracketLeft: [0x1a], BracketRight: [0x1b], Enter: [0x1c], ControlLeft: [0x1d], KeyA: [0x1e],
  KeyS: [0x1f], KeyD: [0x20], KeyF: [0x21], KeyG: [0x22], KeyH: [0x23], KeyJ: [0x24], KeyK: [0x25],
  KeyL: [0x26], Semicolon: [0x27], Quote: [0x28], Backquote: [0x29], ShiftLeft: [0x2a],
  Backslash: [0x2b], KeyZ: [0x2c], KeyX: [0x2d], KeyC: [0x2e], KeyV: [0x2f], KeyB: [0x30],
  KeyN: [0x31], KeyM: [0x32], Comma: [0x33], Period: [0x34], Slash: [0x35], ShiftRight: [0x36],
  AltLeft: [0x38], Space: [0x39], CapsLock: [0x3a], F1: [0x3b], F2: [0x3c], F3: [0x3d], F4: [0x3e],
  F5: [0x3f], F6: [0x40], F7: [0x41], F8: [0x42], F9: [0x43], F10: [0x44], F11: [0x57], F12: [0x58],
  NumpadEnter: [0xe0, 0x1c], ControlRight: [0xe0, 0x1d], AltRight: [0xe0, 0x38], Home: [0xe0, 0x47],
  ArrowUp: [0xe0, 0x48], PageUp: [0xe0, 0x49], ArrowLeft: [0xe0, 0x4b], ArrowRight: [0xe0, 0x4d],
  End: [0xe0, 0x4f], ArrowDown: [0xe0, 0x50], PageDown: [0xe0, 0x51], Insert: [0xe0, 0x52],
  Delete: [0xe0, 0x53],
  // ⌘ is a Ctrl, for a Mac, where Ctrl and an arrow switch desktops.
  MetaLeft: [0x1d], MetaRight: [0xe0, 0x1d],
};

const MOUSE_MS = 30;
const MOUSE = 0xf0;

const hex = (bytes) => bytes.map((b) => b.toString(16).padStart(2, "0")).join("");

export class ScreenView {
  /// `body` is the panel's body; `read` reads a store path at the instant
  /// in view; `type` sends a line to the board's front door; `live` says
  /// whether the page is following the live run, whose frames are sent.
  /// `ask(x, y)`, if given, is asked where a pixel came from: an
  /// Alt-click on the screen.
  constructor(body, { read, type, live = () => false, ask = null }) {
    this.read = read;
    this.ask = ask;
    this.type = type;
    this.live = live;
    this.pushedAt = 0;
    this.frame = null; // the count of the frame drawn
    this.busy = false;
    this.held = new Set();
    this.shown = 0; // frames drawn, and since when, for the rate
    this.since = performance.now();
    body.innerHTML = `<canvas class="screen-canvas" tabindex="0" title="click, then play: the mouse turns, walks and fires (right button strafes); arrows move, Ctrl or ⌘ fires, Space opens, Alt strafes, Shift runs; Esc lets the mouse go, and again for the menu. Alt-click a pixel to ask where it came from"></canvas>
      <div class="dim screen-status">no frame yet</div>`;
    this.canvas = body.querySelector("canvas");
    this.status = body.querySelector(".screen-status");
    this.context = this.canvas.getContext("2d");
    this.canvas.addEventListener("keydown", (event) => this.key(event, true));
    this.canvas.addEventListener("keyup", (event) => this.key(event, false));
    // Keys still down when the screen loses the focus are let go, or the
    // board would hold them down for ever.
    this.canvas.addEventListener("blur", () => {
      for (const code of this.held) this.send(code, false);
      this.held.clear();
    });
    this.canvas.addEventListener("pointerdown", (event) => {
      // Alt-click, the pointer free: where that pixel came from, not a game.
      if (event.altKey && this.ask && document.pointerLockElement !== this.canvas) {
        event.preventDefault();
        const box = this.canvas.getBoundingClientRect();
        const x = Math.floor(((event.clientX - box.left) / box.width) * this.canvas.width);
        const y = Math.floor(((event.clientY - box.top) / box.height) * this.canvas.height);
        return this.ask(x, y);
      }
      this.canvas.focus();
      if (document.pointerLockElement !== this.canvas) this.canvas.requestPointerLock?.();
    });
    this.buttons = 0; // held, as the packet's low bits have them
    this.sentButtons = 0;
    this.moved = [0, 0]; // across and up, not yet sent
    // Browser buttons 0, 1, 2 are left, middle, right; the packet's bits
    // are left, right, middle.
    const bit = (button) => [1, 4, 2][button] ?? 0;
    this.canvas.addEventListener("mousemove", (event) => {
      if (document.pointerLockElement !== this.canvas) return;
      this.moved[0] += event.movementX;
      this.moved[1] -= event.movementY;
    });
    this.canvas.addEventListener("mousedown", (event) => {
      if (document.pointerLockElement !== this.canvas) return;
      event.preventDefault();
      this.buttons |= bit(event.button);
    });
    this.canvas.addEventListener("mouseup", (event) => {
      if (document.pointerLockElement !== this.canvas) return;
      this.buttons &= ~bit(event.button);
    });
    this.canvas.addEventListener("contextmenu", (event) => event.preventDefault());
    // The pointer let go (Esc, another window): its buttons are let go too.
    document.addEventListener("pointerlockchange", () => {
      if (document.pointerLockElement === this.canvas) return;
      this.buttons = 0;
      this.moved = [0, 0];
    });
    setInterval(() => this.sendMouse(), MOUSE_MS);
    const tick = () => {
      this.poll();
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }

  key(event, down) {
    const code = SCAN_CODES[event.code];
    if (!code) return;
    event.preventDefault();
    event.stopPropagation();
    // The board knows a key is held: a repeat is nothing new.
    if (event.repeat) return;
    if (down) this.held.add(event.code);
    else this.held.delete(event.code);
    this.send(event.code, down);
  }

  send(name, down) {
    const code = SCAN_CODES[name];
    const bytes = down ? code : [...code.slice(0, -1), code.at(-1) | 0x80];
    this.type(`keys ${hex(bytes)}`);
  }

  /// A mouse packet, when the mouse has moved or a button changed: at most
  /// a signed byte of movement each way, the rest kept for the next.
  sendMouse() {
    const [x, y] = this.moved;
    if (!x && !y && this.buttons === this.sentButtons) return;
    const clamp = (n) => Math.max(-127, Math.min(127, Math.trunc(n)));
    const [dx, dy] = [clamp(x), clamp(y)];
    this.moved = [x - dx, y - dy];
    this.sentButtons = this.buttons;
    this.type(`keys ${hex([MOUSE | this.buttons, dx & 0xff, dy & 0xff])}`);
  }

  /// A frame the worker sent: drawn if the page is following the live run
  /// and it is newer than the one drawn.
  pushed(value) {
    if (!this.live()) return;
    this.pushedAt = performance.now();
    if (value.frame > (this.frame ?? -1)) this.draw(value);
  }

  /// Asks for a newer frame than the one drawn, one ask at a time: when the
  /// page stands at an instant, or no frame has been sent lately.
  async poll() {
    if (this.busy || (this.live() && performance.now() - this.pushedAt < 250)) return;
    this.busy = true;
    try {
      const answer = await this.read(`board/display/${this.frame ?? 0}`);
      if (answer?.off) this.status.textContent = "the panel is dark";
      else if (answer?.pixels !== undefined) this.draw(answer);
    } catch {
      // No board, or none at this instant: nothing to draw.
    } finally {
      this.busy = false;
    }
  }

  draw({ frame, width, height, format, palette, pixels }) {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
      this.image = this.context.createImageData(width, height);
    }
    const bytes = Uint8Array.from(atob(pixels), (c) => c.charCodeAt(0));
    const out = this.image.data;
    if (format === "indexed") {
      const colours = Uint8Array.from(atob(palette), (c) => c.charCodeAt(0));
      for (let i = 0, o = 0; i < bytes.length; i++, o += 4) {
        const c = 3 * bytes[i];
        out[o] = colours[c];
        out[o + 1] = colours[c + 1];
        out[o + 2] = colours[c + 2];
        out[o + 3] = 255;
      }
    } else {
      for (let i = 0, o = 0; i < bytes.length; i += 3, o += 4) {
        out[o] = bytes[i];
        out[o + 1] = bytes[i + 1];
        out[o + 2] = bytes[i + 2];
        out[o + 3] = 255;
      }
    }
    this.context.putImageData(this.image, 0, 0);
    // A board without a panel never gets this far, and its page never
    // shows the screen.
    this.canvas.closest(".panel").hidden = false;
    this.frame = frame;
    this.shown++;
    const now = performance.now();
    if (now - this.since >= 1000) {
      const rate = (1000 * this.shown) / (now - this.since);
      this.status.textContent = `frame ${frame} · ${rate.toFixed(1)} a second here · ${width}×${height}`;
      this.shown = 0;
      this.since = now;
    }
  }
}
