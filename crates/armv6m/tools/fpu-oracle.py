#!/usr/bin/env python3
"""Writes the FPU corpus: random floating-point instructions, and what QEMU
does with each.

    python3 -I tools/fpu-oracle.py [cases] [seed] > tests/fpu.corpus

Unicorn's Cortex-M7 is single-precision only, so the oracle is its ARMv8
AArch32 core in Thumb state, whose VFP is the same architecture: the same
encodings, the same FPSCR, the same NaN and flush-to-zero rules. It has
thirty-two double registers where an FPv5-D16 has sixteen, and Advanced
SIMD besides, so the generator keeps to what the M7 has.

A line is

    INSTR r0 .. r12 sp lr APSR FPSCR s0 .. s31 => r0 .. r12 sp lr pc APSR FPSCR s0 .. s31 [addr:byte ...]

in hex, or `=> fault`. SRAM starts as `thumb2-oracle.py`'s pattern.
"""

import random
import struct
import sys

import unicorn
from unicorn import arm_const as A

SRAM = 0x2000_0000
SRAM_SIZE = 0x9000
CODE = SRAM + 0x100
# The FPSCR bits M-profile has, less AHP.
FPSCR_MASK = 0xF3C0_009F


def pattern():
    return bytes(((a * 0x9E3779B1) & 0xFFFFFFFF) >> 24 for a in range(SRAM, SRAM + SRAM_SIZE))


def f32(x):
    return struct.unpack("<I", struct.pack("<f", x))[0]


def f64(x):
    return struct.unpack("<Q", struct.pack("<d", x))[0]


def single(rng):
    r = rng.random()
    if r < 0.08:
        return rng.choice([0, 0x80000000])
    if r < 0.14:
        return rng.choice([0x7F800000, 0xFF800000])
    if r < 0.22:
        # NaNs, quiet and signalling, with payloads.
        return rng.getrandbits(1) << 31 | 0x7F800000 | rng.choice([0x400000, 0]) | rng.randrange(1, 0x400000)
    if r < 0.32:
        return rng.getrandbits(1) << 31 | rng.randrange(1, 0x800000)  # denormal
    if r < 0.5:
        return f32(rng.choice([1.0, -1.0, 0.5, 2.0, 3.0, 0.1, 1e10, -7.25, 65504.0, 1.5, 100.0, 2**31, -(2**31), 2**32]))
    if r < 0.6:
        return rng.getrandbits(1) << 31 | rng.choice([0x7F7FFFFF, 0x00800000, 0x3F800001, 0x4B000000])
    return rng.getrandbits(32)


def double(rng):
    r = rng.random()
    if r < 0.08:
        return rng.choice([0, 1 << 63])
    if r < 0.14:
        return rng.getrandbits(1) << 63 | 0x7FF << 52
    if r < 0.22:
        return rng.getrandbits(1) << 63 | 0x7FF << 52 | rng.choice([1 << 51, 0]) | rng.randrange(1, 1 << 51)
    if r < 0.32:
        return rng.getrandbits(1) << 63 | rng.randrange(1, 1 << 52)
    if r < 0.5:
        return f64(rng.choice([1.0, -1.0, 0.5, 2.0, 3.0, 0.1, 1e300, -7.25, 1e-310, 1.5, 2**31, -(2**31), 2**32, 2**63]))
    if r < 0.6:
        return rng.getrandbits(1) << 63 | rng.choice([0x7FEFFFFFFFFFFFFF, 1 << 52, 0x3FF0000000000001])
    return rng.getrandbits(64)


def registers(rng):
    s = []
    for _ in range(16):
        if rng.random() < 0.5:
            v = double(rng)
            s += [v & 0xFFFFFFFF, v >> 32]
        else:
            s += [single(rng), single(rng)]
    return s


def value(rng):
    r = rng.random()
    if r < 0.4:
        return rng.randrange(SRAM + 0x1000, SRAM + 0x8000) & ~3
    if r < 0.5:
        return rng.randrange(SRAM + 0x1000, SRAM + 0x8000)
    if r < 0.65:
        return rng.choice([0, 1, 2, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF, 0xFFFF, 0x8000, 100])
    return rng.getrandbits(32)


def instruction(rng):
    """A random encoding in the FPU's space, kept to an FPv5-D16."""
    kind = rng.random()
    sz = rng.getrandbits(1)
    hw2 = rng.getrandbits(16) & ~0x0F00 | 0x0A00 | sz << 8
    if kind < 0.6:
        # Data processing, and the 32-bit transfers.
        hw1 = 0xEE00 | rng.getrandbits(8)
        if hw2 & 0x10:
            # Transfers: VMRS and VMSR name FPSCR, and a scalar is a word.
            a = hw1 >> 5 & 7
            if hw2 & 0x100:
                hw1 &= ~0xC0
                hw2 &= ~0xE0
                hw2 &= ~0x80  # D16
            elif a == 0b111:
                hw1 = hw1 & 0xFFF0 | 1
                hw2 &= ~0xEF
            hw2 &= ~0xF
            if hw1 & 0x10 and hw2 >> 12 == 0xF and not (a == 0b111 and not hw2 & 0x100):
                return None  # VMOV to the pc, UNPREDICTABLE
        elif sz:
            hw1 &= ~0x40
            hw2 &= ~0x20
            if hw1 & 0xB0 != 0xB0:
                hw2 &= ~0x80
        elif hw1 & 0xBF == 0xB7 and hw2 & 0xC0 == 0xC0:
            hw1 &= ~0x40  # VCVT.F64.F32: a double destination
        if hw1 & 0xBF == 0xB9:
            return None  # VJCVT, ARMv8.3's, A-profile only
        if hw1 & 0xBA == 0xBA and hw2 & 0x50 == 0x40 and not hw2 & 0x80:
            # A 16-bit fixed-point conversion with more fraction bits than
            # the field has, which is UNPREDICTABLE.
            if (hw2 & 0xF) << 1 | (hw2 >> 5 & 1) > 16:
                return None
    elif kind < 0.85:
        # Loads, stores and the 64-bit transfers.
        hw1 = 0xEC00 | rng.getrandbits(9)
        if hw1 & 0x1E0 == 0x40 and not sz and (hw2 & 0xF) << 1 | (hw2 >> 5 & 1) == 31:
            return None  # two singles from s31, which have no s32
        if sz:
            hw1 &= ~0x40
            if (hw1 & 0x1E0) == 0x40:
                hw2 &= ~0x20
            first = hw2 >> 12
            if (hw1 & 0x1E0) != 0x40 and first + (hw2 & 0xFF) // 2 > 16:
                hw2 = hw2 & ~0xFF | rng.randrange(2, 2 * (16 - first) + 1, 2)
    else:
        # FPv5's unconditional ones.
        hw1 = 0xFE00 | rng.getrandbits(8)
        hw2 &= ~0x10
        if sz:
            hw1 &= ~0x40
            hw2 &= ~0x20
            if not (hw1 & 0x80 and hw1 & 0x30 == 0x30):
                hw2 &= ~0x80
    return hw1, hw2


def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 3000
    seed = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    rng = random.Random(seed)
    base = pattern()
    uc = unicorn.Uc(unicorn.UC_ARCH_ARM, unicorn.UC_MODE_THUMB)
    uc.ctl_set_cpu_model(A.UC_CPU_ARM_MAX)
    uc.mem_map(SRAM, SRAM_SIZE)
    taken = []
    uc.hook_add(unicorn.UC_HOOK_INTR, lambda uc, n, _: (taken.append(n), uc.emu_stop()))
    regs = [getattr(A, f"UC_ARM_REG_R{n}") for n in range(13)]
    sregs = [getattr(A, f"UC_ARM_REG_S{n}") for n in range(32)]
    clean = uc.context_save()
    written = attempts = 0
    while written < count and attempts < count * 50:
        attempts += 1
        chosen = instruction(rng)
        if chosen is None:
            continue
        hw1, hw2 = chosen
        code = hw1.to_bytes(2, "little") + hw2.to_bytes(2, "little")
        before = [value(rng) for _ in range(13)]
        sp = SRAM + 0x4000 + 4 * rng.randrange(0, 0x400)
        lr = value(rng)
        flags = rng.getrandbits(4) << 28
        fpscr = rng.getrandbits(32) & FPSCR_MASK
        s = registers(rng)
        memory = bytearray(base)
        memory[CODE - SRAM : CODE - SRAM + 4] = code
        uc.context_restore(clean)
        uc.ctl_flush_tb()
        uc.mem_write(SRAM, bytes(memory))
        uc.reg_write(A.UC_ARM_REG_C1_C0_2, 0xF << 20)
        uc.reg_write(A.UC_ARM_REG_FPEXC, 1 << 30)
        uc.reg_write(A.UC_ARM_REG_CPSR, 0x30 | flags)
        for reg, v in zip(regs, before):
            uc.reg_write(reg, v)
        uc.reg_write(A.UC_ARM_REG_SP, sp)
        uc.reg_write(A.UC_ARM_REG_LR, lr)
        uc.reg_write(A.UC_ARM_REG_FPSCR, fpscr)
        for reg, v in zip(sregs, s):
            uc.reg_write(reg, v)
        line = (
            f"{hw1:04x}{hw2:04x} "
            + " ".join(f"{v:x}" for v in before)
            + f" {sp:x} {lr:x} {flags:x} {fpscr:x} "
            + " ".join(f"{v:x}" for v in s)
            + " =>"
        )
        taken.clear()
        try:
            uc.emu_start(CODE | 1, 0xFFFFFFFF, count=1)
        except unicorn.UcError as error:
            if error.errno in (unicorn.UC_ERR_READ_UNMAPPED, unicorn.UC_ERR_WRITE_UNMAPPED, unicorn.UC_ERR_FETCH_UNMAPPED):
                continue
            if uc.reg_read(A.UC_ARM_REG_PC) == CODE:
                print(f"{line} fault")
                written += 1
                continue
        pc = uc.reg_read(A.UC_ARM_REG_PC)
        if taken and pc == CODE:
            print(f"{line} fault")
            written += 1
            continue
        after = [uc.reg_read(r) for r in regs]
        cpsr = uc.reg_read(A.UC_ARM_REG_CPSR)
        out_s = [uc.reg_read(r) for r in sregs]
        final = uc.mem_read(SRAM, SRAM_SIZE)
        changes = [f"{SRAM + a:x}:{final[a]:02x}" for a in range(SRAM_SIZE) if final[a] != memory[a]]
        out = " ".join(f"{v:x}" for v in after)
        out += f" {uc.reg_read(A.UC_ARM_REG_SP):x} {uc.reg_read(A.UC_ARM_REG_LR):x} {pc:x} {cpsr & 0xF0000000:x}"
        out += f" {uc.reg_read(A.UC_ARM_REG_FPSCR) & FPSCR_MASK:x} " + " ".join(f"{v:x}" for v in out_s)
        print(f"{line} {out}" + (" " + " ".join(changes) if changes else ""))
        written += 1


main()
