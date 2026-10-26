# The Hubris bench

[Hubris](https://github.com/oxidecomputer/hubris), Oxide's microcontroller
operating system, running on an emulated STM32H753 Nucleo-144 — the
`demo-stm32h753-nucleo` app, all 24 tasks, unmodified — with
[Humility](https://github.com/oxidecomputer/humility), its debugger,
attached to it, all inside one zaqaru container, in a browser. The G0
Nucleos are there too, to flash onto the bench.

```
browser: a terminal and a board panel
 └─ zaqaru module (wasm)
     ├─ kernel
     │   ├─ board: Cortex-M7 + STM32H753, running Hubris's H753 demo image
     │   │   ├─ debug port: SW-DP v2; AP0 the M7's AHB-AP, AP1 D3's, AP2 the debug APB
     │   │   │   ├─ CMSIS-DAP v2 probe, served as a USB device (services/usb)
     │   │   │   └─ GDB remote server on 127.0.0.1:3333
     │   │   ├─ I2C2: a TMP117 at 0x48, an AT24C02 at 0x50
     │   │   ├─ SPI1: a W25Q80 flash; SPI3: a root of trust, speaking sprot
     │   │   └─ Ethernet, cabled to a host: IPv6, UDP, ping
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
board beside it, drawn from what it says of itself: its chip with every
GPIO pin coloured by its mode and level as the firmware drives it, its
LEDs and button, its buses with the parts on them, the Ethernet jack, the
serial port and the debug probe. "show the machine" opens the rest of
the debugger — processes, files, the timeline — and the slider seeks the
whole bench, board included, to any instant of the run.

## Probes

Click anything on the drawn board to put a probe on it, as you would clip
a logic analyzer on, and the panel shows what crosses it, in the board's
simulated time:

| probe on | shows |
|----------|-------|
| a pin | its level as a waveform, over a window from 1 ms to 10 s, and its latest edges: an LED's blink, a chip-select, ROT_IRQ, the button |
| an I2C bus | its transactions, decoded: `0x48 W 00 · 0x48 R 0c 80` — Humility's read of the TMP117, through hiffy and the I2C driver |
| a SPI bus | each chip-select's transfer, what went out and what came back: the flash's JEDEC ID, the RoT's sprot messages, the KSZ8463's registers |
| the serial port | its bytes as text, each way |
| the Ethernet jack | its frames, summarized: addresses, VLAN, IPv6, UDP ports or ICMPv6 |
| a part | what it holds: the TMP117's temperature, the EEPROM's bytes, the RoT's state and last request, the switch's ports and VLANs |

The probes read the board's trace (`board/trace/{since}/{channels}` in the
container's store), which the kernel keeps as the board runs: so a probe
put on shows the recent past, and pausing the board's clock or moving it
on by a millisecond (`pause`, `advance 1ms`, or the panel's buttons) holds
a transaction still to read. A Gimletlet or a PSC flashed onto the bench is
drawn as itself, with its own LEDs and parts.

`build.sh` builds Hubris's `app/demo-stm32h7-nucleo/app-h753.toml`, and
the G031 and G070 demos to flash in its place, on this host. It builds
Humility (pinned, as released, with its default features),
`zaqaru-m0` and zaqaru as Linux x86-64 programs, natively on such a host and
otherwise in a container from `builder.Dockerfile`. Then it builds the
bench image from `Dockerfile`, bakes it, and checks the module under Node
(`check.mjs`).

## USB, and a probe of your own

The page's **usb** panel lists the USB devices the container's programs can
open, and what each has carried: totals, and the latest transfers decoded
as CMSIS-DAP commands — `Transfer: AP write TAR, AP read DRW`, a
`TransferBlock` of memory, an `Info` query. It is read from the container's
store like every other panel, so the slider shows the traffic as it was at
any instant.

**connect a USB device…** grants the container a device from your computer
through WebUSB (Chrome or Edge). The page asks; the worker the container
runs in attaches what was granted and serves it at `services/usb` — the
same store contract the board's probe is served through
(`web/usb.js`, `crates/kernel/src/usb.rs`) — so the next program to start
finds it in sysfs beside the board's. With two probes on the desk, name the
one you mean, as on a real one: the door runs `humility` against the
board's probe by its IDs (`1209:0001`), and `humility -p VID:PID …` reaches
yours.

This has been checked against a mock CMSIS-DAP device standing in for
WebUSB, with stock Humility enumerating it, reading its descriptors and
exchanging commands with it; not yet against a real probe. A Nucleo's own
ST-LINK speaks ST-LINK rather than CMSIS-DAP, which probe-rs also drives,
but whether a browser lets it be claimed is untested.

## Things to type

| line | what it does |
| --- | --- |
| `help` | this list |
| `humility tasks` | Hubris's tasks, their state, and what each waits on |
| `humility tasks -s udpecho` | a task's stack, unwound through its frames |
| `humility map` | the memory regions each task can reach, from the archive |
| `humility readvar TICKS` | a kernel variable, read live |
| `humility ringbuf` | the ring buffers |
| `humility hiffy -c UserLeds.led_toggle -a index=2` | an IPC call made on the board, through the hiffy task: the yellow LED changes |
| `humility i2c -c 2 -p F --scan` | a scan of I2C2, through hiffy and the I2C driver: the TMP117 at 0x48 and the EEPROM at 0x50 |
| `humility i2c -c 2 -p F -d 0x48 -r 0 -n 2` | the TMP117's temperature register, in 1/128 °C: `0x0c 0x80` is 25 °C |
| `temperature 31.5` | what the TMP117 reads from now on |
| `humility spi -p 1 -w 0x9f,0,0,0 -r -n 4` | the W25Q80's JEDEC ID, `ef 40 14`, through the SPI driver |
| `humility hiffy -c SpRot.status` | sprot's status call: a request over SPI3, ROT_IRQ, and the RoT's response |
| `humility hiffy -c SpRot.rot_state` | the RoT's boot state: slot A active and valid, slot B erased; `rot_boot_info`, `block_size` and `io_stats` answer too |
| `humility hiffy -c SpRot.read_caboose_region -a slot=A,offset=0 -n 140` | the RoT image's caboose, TLV-C as Hubris writes one |
| `humility hiffy -T 15000 -c Update.prep_image_update` | the update server erases flash bank 2: five seconds of the board's time, as the part's datasheet has it, past hiffy's default timeout |
| `humility flash -F` | flashes the archive's image onto the board through probe-rs's own flash algorithm, then resets it |
| `humility flash --verify` | reads the flash back and compares it with the archive |
| `udp hello` | a datagram to udpecho's port 7, from the host on the board's Ethernet cable, and its echo |
| `udp 998 TEXT` | the same to any port |
| `ping` | an ICMPv6 echo request to the board, and its reply |
| `button` | presses and releases B1, PC13: the user_button task toggles the red LED |
| `archives` | the Hubris builds there are to flash: each one's app and board, from its `app.toml`. The terminal's *example* picker lists them, and choosing one runs `flash` with it |
| `flash gimletlet` | Oxide's Gimletlet 2, with its NIC's KSZ8463 switch and its RoT wired: its network comes up through the switch's VLANs, and `udp hello` and `ping` reach it |
| `flash psc` | Oxide's PSC rev C, with its revision straps driven and its KSZ8463, FRAM, RoT and temperature sensor wired; the framulator's FRAM test passes. Its management network waits for power and PHYs (VSC85x2) the bench does not have |
| `flash g031` | makes the board the part the archive was built for, erased, and flashes it — `g031`, `g070`, `h753`, or one uploaded |
| `humility -p VID:PID …` | the same, through a probe connected from the page |
| `clock` | the board's simulated time, its speed, and the clocks RCC gives out |
| `speed 0.1`, `speed 10`, `speed max` | run the board's time slower or faster than real time, or as fast as it can with its idle time skipped |
| `pause`, `advance 10ms`, `advance next`, `resume` | stop the board's time, move it on by a time or to its next event, and let it go; the board panel has buttons for these, and GDB has them as `monitor` commands |
| `board` | the board itself: running or halted, simulated time, LEDs, faults, exceptions taken, the network |
| `exit` | end the session |

There is no shell behind the prompt: Humility, the board and nothing else.
Outside the page, `zaqaru run web/demo/hubris.wasm -p 8080:80` and
`nc localhost 8080` give the same session.

## What is and is not modelled

- The ARMv7E-M instruction set of a Cortex-M7, Thumb-2 and DSP, and its
  FPv5 double-precision FPU, checked against QEMU's; and the whole ARMv6-M
  instruction set for the G0s. The system model of each: exceptions,
  priorities and their grouping, BASEPRI and FAULTMASK, the configurable
  faults and their status registers, both stacks, privilege, SysTick, the
  NVIC, the MPU. Tasks run unprivileged under it, as on the part.
- Of the STM32H753: its memories (ITCM, DTCM, AXI SRAM, SRAM1–4, backup
  SRAM, two flash banks); PWR, RCC and flash wait states with everything
  startup waits on settled at once; GPIO A–K, EXTI and SYSCFG; the USARTs;
  I2C1–4 and SPI1–6, with the parts the board file wires to them; the
  general-purpose timers as time bases; the RNG, deterministic; DBGMCU; the
  dual-bank flash interface — flash-word programming, sector and bank
  erase, the option bytes' bank swap; and Ethernet — the MAC, its DMA and
  descriptor rings, MDIO to a LAN8742A with its link up. Every other
  peripheral register reads back what was written.
- Of the STM32G0s: RCC, GPIO, the USARTs and the flash interface.
- What is wired to the H7's buses, from the board file's `i2c` and `spi`
  lines — or, for an Oxide board flashed onto the bench, the parts that
  board has (and the straps it drives), from the door's table of them: a TMP117 temperature sensor, an AT24C02 EEPROM, a W25Q80 SPI
  flash (JEDEC and unique IDs, reads, page programs, erases), and a root
  of trust that answers the SP-to-RoT protocol as the LPC55's sprot server
  does — status, I/O counters, lifecycle state, boot state and boot
  information, the update block size, and its images' cabooses; it refuses
  updates, attestation and the rest as unknown messages, as an LPC55 built
  without them does; a KSZ8463 Ethernet switch, whose VLAN table and port
  VLAN IDs tag and untag the frames between the MAC and the cable, as it
  sits between them on a Gimletlet; and an MB85RS64T FRAM. The MAC inserts
  VLAN tags from its context descriptors and strips them into RDES0. A SPI
  part is selected by its chip-select pin, driven low as a GPIO output.
- The debug architecture: each part's debug port and APs, the ROM tables
  and CoreSight IDs, DHCSR halting and stepping, DCRSR register transfer,
  DEMCR vector catch, DFSR, the breakpoint unit (an M7's FPB, an M0+'s
  BPU), the DWT's registers and an M7's cycle counter. Watchpoints never
  match.
- The network: the board's Ethernet is cabled to one host, which speaks
  IPv6 on its link-local address — neighbour discovery, echo, UDP — and is
  driven by the door's `udp` and `ping`. The container's own programs have
  no IPv6 sockets to reach it with.
- Time: simulated time is kept in picoseconds. Each instruction costs
  cycles of the core's clock as RCC sets it — the clock tree worked out
  from its registers, HSI and the PLLs through the prescalers, so Hubris's
  H7 startup takes the core from 64 to 400 MHz — and those cycles are the
  instruction's own and its stalls: a load's or store's, a taken branch's
  refill, a divide's, exception entry and return, and the wait states of
  the bus an access goes to at its clocks, flash's latency and the SRAMs'
  hidden once the M7's caches are on. The peripherals take their time at
  their kernel clocks: a USART byte its frame at its baud, an I2C byte its
  nine SCL periods, a SPI frame its SCK bits, an Ethernet frame its wire
  time, a flash word or sector or bank its datasheet's programming or
  erase time; and the parts on the buses theirs. The board runs at real
  time against the container's clock by default, or slower or faster, or
  as fast as it can; an H7 in a browser falls behind real time when it is
  busy, and the kernel gives the difference up rather than racing. Not
  modelled: the M7's dual issue and branch prediction, cache misses.
- Not modelled: caches (they are registers that read back), the H7's
  other DMA engines, QSPI, the hash and crypto blocks, USB, and anything
  analog.
