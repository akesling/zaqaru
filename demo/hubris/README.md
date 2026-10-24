# The Hubris bench

[Hubris](https://github.com/oxidecomputer/hubris), Oxide's microcontroller
operating system, running on an emulated STM32G031 Nucleo, with
[Humility](https://github.com/oxidecomputer/humility), its debugger,
attached to it, all inside one zaqaru container, in a browser.

```
browser: a terminal and a board panel
 └─ zaqaru module (wasm)
     ├─ kernel
     │   └─ board: Cortex-M0+ + STM32G031, running Hubris's G031 demo image
     │       └─ probe: GDB remote server on 127.0.0.1:3333
     └─ processes (x86-64, interpreted)
         ├─ zaqaru-m0 door   port 80: a session, a line at a time
         └─ humility         --probe gdb:127.0.0.1:3333, one per command
```

The board is not a process. An image that contains `/etc/zaqaru/board`
gets one from the kernel (see `crates/kernel/src/board.rs`): the
`armv6m` crate's machine, run beside the processes against the
container's own clock, with its probe as a listener the kernel holds. So
the board costs a few host instructions per ARM instruction, not an
interpreter's worth of x86 per ARM instruction, and it keeps time the way
the container does.

## Make it

```sh
demo/hubris/build.sh ../hubris            # a Hubris checkout; writes web/demo/hubris.wasm
python3 -m http.server -d . 8000
# http://localhost:8000/web/?module=demo/hubris.wasm&live=80&terminal=80
```

The page opens on a terminal, in session with the container, and the
board beside it: its LED drawn as PC6 drives it, its state, time and
registers, read from the container's store. "show the machine" opens the
rest of the debugger — processes, files, the timeline — and the slider
seeks the whole bench, board included, to any instant of the run.

`build.sh` builds Hubris's `app/demo-stm32g0-nucleo/app-g031.toml` on this
host. It builds Humility (pinned, with `humility-gdb-probe.patch`),
`zaqaru-m0` and zaqaru as Linux x86-64 programs, natively on such a host and
otherwise in a container from `builder.Dockerfile`. Then it builds the
bench image from `Dockerfile`, bakes it, and checks the module under Node
(`check.mjs`).

## Things to type

| line | what it does |
| --- | --- |
| `help` | this list |
| `humility tasks` | Hubris's tasks, their state, and what each waits on |
| `humility tasks -s pong` | a task's stack, unwound through its frames |
| `humility map` | the memory regions each task can reach, from the archive |
| `humility readvar TICKS` | a kernel variable, read live |
| `humility ringbuf` | the ring buffers |
| `humility hiffy -c UserLeds.led_toggle -a index=0` | an IPC call made on the board, through the hiffy task: the LED changes |
| `board` | the board itself: running or halted, simulated time, LED, faults, exceptions taken |
| `exit` | end the session |

There is no shell behind the prompt: Humility, the board and nothing else.
Outside the page, `zaqaru run web/demo/hubris.wasm -p 8080:80` and
`nc localhost 8080` give the same session.

## What is and is not modelled

- The whole ARMv6-M instruction set and system model: exceptions,
  priorities, both stacks, privilege, SysTick, the NVIC, the MPU. Tasks
  run unprivileged under it, as on the part.
- Of the STM32G031: RCC with its oscillators settled, and GPIO. Every
  other peripheral register reads back what was written.
- Time: one instruction is one cycle of the 16 MHz clock Hubris configures,
  and the board runs at that rate against the container's clock. Exception
  entry costs no cycles.
- No UART, no interrupt-raising peripherals, no flash programming.
