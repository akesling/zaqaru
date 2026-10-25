# The Hubris bench

[Hubris](https://github.com/oxidecomputer/hubris), Oxide's microcontroller
operating system, running on an emulated STM32G031 Nucleo, with
[Humility](https://github.com/oxidecomputer/humility), its debugger,
attached to it, all inside one zaqaru container, in a browser.

```
browser: a terminal and a board panel
 └─ zaqaru module (wasm)
     ├─ kernel
     │   ├─ board: Cortex-M0+ + STM32G031, running Hubris's G031 demo image
     │   │   └─ debug port: SW-DP and AHB-AP
     │   │       ├─ CMSIS-DAP v2 probe, served as a USB device (services/usb)
     │   │       └─ GDB remote server on 127.0.0.1:3333
     │   └─ usbfs and sysfs: /dev/bus/usb/001/002, /sys/bus/usb/devices/1-1
     └─ processes (x86-64, interpreted)
         ├─ zaqaru-m0 door   port 80: a session, a line at a time
         └─ humility         stock, one per command; probe-rs finds the probe
```

The board is not a process. An image that contains `/etc/zaqaru/board`
gets one from the kernel (see `crates/kernel/src/board.rs`): the
`armv6m` crate's machine, run beside the processes against the
container's own clock. So the board costs a few host instructions per ARM
instruction, not an interpreter's worth of x86 per ARM instruction, and it
keeps time the way the container does.

Humility is Oxide's, unmodified. It reaches the board the way it reaches a
real one: probe-rs lists the USB devices in sysfs, finds an interface named
CMSIS-DAP, opens it through usbfs and speaks CMSIS-DAP to it. The probe's
firmware (`crates/armv6m/src/cmsis_dap.rs`) turns those commands into
accesses to the chip's debug port (`dap.rs`) and its debug registers
(`debug.rs`) — halting, register transfer, breakpoints, vector catch — as
on the part. The kernel serves the probe as a USB device through the same
store contract a host would grant one through (`crates/kernel/src/usb.rs`),
so a host can hand a container real devices the same way.

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
host. It builds Humility (pinned, as released, with its default features),
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
| `humility flash -F` | flashes the archive's image onto the board through probe-rs's own flash algorithm, then resets it |
| `humility flash --verify` | reads the flash back and compares it with the archive |
| `board` | the board itself: running or halted, simulated time, LED, faults, exceptions taken |
| `exit` | end the session |

There is no shell behind the prompt: Humility, the board and nothing else.
Outside the page, `zaqaru run web/demo/hubris.wasm -p 8080:80` and
`nc localhost 8080` give the same session.

## What is and is not modelled

- The whole ARMv6-M instruction set and system model: exceptions,
  priorities, both stacks, privilege, SysTick, the NVIC, the MPU. Tasks
  run unprivileged under it, as on the part.
- Of the STM32G031: RCC with its oscillators settled, GPIO, and the flash
  interface — unlocking, page and mass erase, double-word programming, the
  status and error flags — with FLASHSIZE in system memory. Every other
  peripheral register reads back what was written.
- The debug architecture: an ADIv5 SW-DP and AHB-AP, the ROM table and
  CoreSight IDs, DHCSR halting and stepping, DCRSR register transfer,
  DEMCR vector catch, DFSR, the four BPU comparators. The DWT's registers
  read back, and its comparators never match.
- Time: one instruction is one cycle of the 16 MHz clock Hubris configures,
  and the board runs at that rate against the container's clock. Exception
  entry costs no cycles.
- No UART, no interrupt-raising peripherals, no option-byte programming.
