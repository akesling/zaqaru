#!/usr/bin/env python3
"""Writes the Thumb-2 corpus: random instructions, and what a Cortex-M7 does
with each, according to Unicorn (QEMU's Thumb-2).

    python3 -I tools/thumb2-oracle.py [cases] [seed] > tests/thumb2.corpus

Each case is one instruction executed once from a known state: random
registers, half of them pointing into SRAM so that loads and stores land
somewhere, random flags, and SRAM filled with a pattern both sides compute.
A line is

    INSTR r0 .. r12 sp lr APSR => r0 .. r12 sp lr pc APSR [addr:byte ...]

with every number in hex, or `=> fault` for an instruction the M7 refused.
The bytes are SRAM that changed. An INSTR of several, joined by `+`, is an
IT instruction and the block it governs, run as many instructions as it
holds: it runs until the pc reaches the end of them, because Unicorn's
instruction count does not count an IT block's the way the architecture
does.

QEMU's *instruction* semantics are the oracle; its system model is not —
which is the Hubris FAQ's complaint — so nothing here touches exceptions,
special registers, or the coprocessor. Cases that wander off the mapped
memory are dropped, since what they would test is the memory map.
"""

import random
import sys

import unicorn
from unicorn import arm_const as A

SRAM = 0x2000_0000
SRAM_SIZE = 0x9000  # an STM32G070's 36 KiB
CODE = SRAM + 0x100
# `msr apsr_nzcvqg, r0`: how the flags are set, since writing xPSR from
# outside sets everything but GE.
FLAGS_STUB = SRAM + 0x80
SP_REG = A.UC_ARM_REG_SP


def pattern():
    """SRAM's contents before every case, which the test computes too."""
    return bytes(((a * 0x9E3779B1) & 0xFFFFFFFF) >> 24 for a in range(SRAM, SRAM + SRAM_SIZE))


INTERESTING = [0, 1, 2, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF, 0xFFFF, 0x8000, 0x7FFF, 0xFF, 0x80, 0x10000, 0xFFFF8000]


def value(rng):
    r = rng.random()
    if r < 0.3:
        return rng.randrange(SRAM + 0x1000, SRAM + 0x8000) & ~7
    if r < 0.45:
        return rng.randrange(SRAM + 0x1000, SRAM + 0x8000)
    if r < 0.6:
        return rng.choice(INTERESTING)
    if r < 0.7:
        return rng.randrange(0, 64)
    return rng.getrandbits(32)


def wanted16(i):
    """16-bit encodings worth asking QEMU about."""
    if i >> 11 >= 0b11101:
        return False  # the first half of a 32-bit one
    if i & 0xFF00 in (0xBE00, 0xDF00, 0xDE00):
        return False  # BKPT, SVC, UDF: exceptions
    if i & 0xFFE0 == 0xB660:
        return False  # CPS
    if i & 0xFF00 == 0xBF00:
        return False  # IT and the hints, tested on their own
    if i & 0xFF00 == 0x4700:
        return False  # BX and BLX, which in Handler mode can return
    if i & 0xFFE0 == 0xB640:
        return False  # SETEND, A-profile only, which Unicorn executes
    return True


def wanted32(hw1, hw2):
    op1 = hw1 >> 11 & 3
    op2 = hw1 >> 4 & 0x7F
    if op1 == 0b01 and op2 & 0x40 or op1 == 0b11 and op2 & 0x40:
        return False  # coprocessor
    if op1 == 0b11 and op2 & 0b1110001 == 0b0010000:
        return False  # Advanced SIMD loads and stores, which M-profile lacks
    if op1 == 0b11 and op2 & 0b1110001 == 0 and hw1 & 0xF == 0xF:
        return False  # a store to the pc, UNDEFINED, which Unicorn performs
    if op1 == 0b01 and op2 & 0b1111010 == 0b0001000 and hw2 >> 4 & 0xF not in (0, 1, 4, 5):
        return False  # ARMv8-M's acquire and release, which Unicorn's M7 has
    exclusive = op1 == 0b01 and op2 & 0b1100100 == 0b0000100 and op2 >> 3 & 3 < 2 and op2 & 3 < 2
    if exclusive and hw2 >> 12 == 0xF and not (op2 & 0b11000 == 0b01000 and op2 & 1 and hw2 >> 4 & 0xF < 2):
        return False  # an exclusive of the pc, which Unicorn does erratically
    dual = op1 == 0b01 and op2 & 0b1100100 == 0b0000100 and not (op2 >> 3 & 3 < 2 and op2 & 3 < 2)
    if dual and hw1 & 0xF == 0xF and hw1 & 0x130 != 0x110:
        return False  # LDRD and STRD on the pc: only the literal load is defined
    if op1 == 0b10 and hw2 & 0x8000:
        # Branches and control: only the branches themselves.
        if hw2 >> 12 & 0b101 == 0 and hw1 >> 7 & 7 != 7:
            return True  # B<cond>.W
        return hw2 >> 12 & 0b101 in (0b001, 0b101)  # B.W, BL
    return True


def it_block(rng):
    """IT with a random condition and mask, and the data processing it
    governs: 16-bit forms, whose flags depend on being in the block, and
    32-bit ones with a modified immediate."""
    firstcond = rng.randrange(0, 14)
    mask = rng.randrange(1, 16)
    length = 4 - ((mask & -mask).bit_length() - 1)
    words = [f"{0xBF00 | firstcond << 4 | mask:04x}"]
    code = (0xBF00 | firstcond << 4 | mask).to_bytes(2, "little")
    for _ in range(length):
        if rng.random() < 0.7:
            i = rng.randrange(0, 0x4400)
            words.append(f"{i:04x}")
            code += i.to_bytes(2, "little")
        else:
            hw1 = 0xF000 | rng.getrandbits(1) << 10 | rng.choice([0, 1, 2, 3, 4, 8, 10, 11, 13, 14]) << 5 | rng.getrandbits(5)
            # Never to the pc, which would branch out of the block, nor the
            # sp, whose low bits QEMU keeps and this model may not.
            hw2 = rng.getrandbits(15) & ~0x0F00 | rng.randrange(0, 13) << 8
            words.append(f"{hw1:04x}{hw2:04x}")
            code += hw1.to_bytes(2, "little") + hw2.to_bytes(2, "little")
    return "+".join(words), code, 1 + length


def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 4000
    seed = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    rng = random.Random(seed)
    base = pattern()
    uc = unicorn.Uc(unicorn.UC_ARCH_ARM, unicorn.UC_MODE_THUMB | unicorn.UC_MODE_MCLASS)
    uc.ctl_set_cpu_model(A.UC_CPU_ARM_CORTEX_M7)
    uc.mem_map(SRAM, SRAM_SIZE)
    # An exception is where QEMU stops: at the faulting instruction, or —
    # for a prefetch abort after a branch into somewhere unexecutable — at
    # the branch's target, the branch done.
    taken = []
    uc.hook_add(unicorn.UC_HOOK_INTR, lambda uc, n, _: (taken.append(n), uc.emu_stop()))
    regs = [getattr(A, f"UC_ARM_REG_R{n}") for n in range(13)]
    # A fresh processor for every case: one that faulted can be left
    # in a state the next would start from.
    clean = uc.context_save()
    written = 0
    attempts = 0
    while written < count and attempts < count * 50:
        attempts += 1
        steps = 1
        if rng.random() < 0.1:
            instr, code, steps = it_block(rng)
        elif rng.random() < 0.3:
            i = rng.getrandbits(16)
            if not wanted16(i):
                continue
            code = i.to_bytes(2, "little")
            instr = f"{i:04x}"
        else:
            hw1 = rng.choice([0b11101, 0b11110, 0b11111]) << 11 | rng.getrandbits(11)
            hw2 = rng.getrandbits(16)
            if not wanted32(hw1, hw2):
                continue
            code = hw1.to_bytes(2, "little") + hw2.to_bytes(2, "little")
            instr = f"{hw1:04x}{hw2:04x}"
        before = [value(rng) for _ in range(13)]
        sp = SRAM + 0x4000 + 4 * rng.randrange(0, 0x400)
        lr = value(rng)
        flags = rng.getrandbits(5) << 27 | rng.getrandbits(4) << 16
        memory = bytearray(base)
        memory[CODE - SRAM : CODE - SRAM + len(code)] = code
        uc.context_restore(clean)
        # Unicorn keeps translated code across writes to it.
        uc.ctl_flush_tb()
        uc.mem_write(SRAM, bytes(memory))
        uc.mem_write(FLAGS_STUB, bytes.fromhex("80f3008c"))
        uc.reg_write(A.UC_ARM_REG_XPSR, 1 << 24)
        uc.reg_write(regs[0], flags)
        uc.emu_start(FLAGS_STUB | 1, 0xFFFFFFFF, count=1)
        assert uc.reg_read(A.UC_ARM_REG_XPSR) & 0xF80F0000 == flags
        uc.mem_write(FLAGS_STUB, bytes(memory[FLAGS_STUB - SRAM : FLAGS_STUB - SRAM + 4]))
        for reg, v in zip(regs, before):
            uc.reg_write(reg, v)
        uc.reg_write(SP_REG, sp)
        uc.reg_write(A.UC_ARM_REG_LR, lr)
        line = f"{instr} " + " ".join(f"{v:x}" for v in before) + f" {sp:x} {lr:x} {flags:x} =>"
        taken.clear()
        try:
            if steps > 1:
                uc.emu_start(CODE | 1, CODE + len(code), count=32)
            else:
                uc.emu_start(CODE | 1, 0xFFFFFFFF, count=1)
        except unicorn.UcError as error:
            if error.errno in (unicorn.UC_ERR_READ_UNMAPPED, unicorn.UC_ERR_WRITE_UNMAPPED, unicorn.UC_ERR_FETCH_UNMAPPED):
                continue
            if uc.reg_read(A.UC_ARM_REG_PC) == CODE:
                print(f"{line} fault")
                written += 1
                continue
            # Done, and what refused was the instruction after it — the
            # one a branch into ARM state landed on.
        after = [uc.reg_read(r) for r in regs]
        pc = uc.reg_read(A.UC_ARM_REG_PC)
        if taken and pc == CODE:
            # QEMU took an exception and stopped where it began.
            print(f"{line} fault")
            written += 1
            continue
        xpsr = uc.reg_read(A.UC_ARM_REG_XPSR)
        final = uc.mem_read(SRAM, SRAM_SIZE)
        changes = [f"{SRAM + a:x}:{final[a]:02x}" for a in range(SRAM_SIZE) if final[a] != memory[a]]
        out = " ".join(f"{v:x}" for v in after)
        out += f" {uc.reg_read(SP_REG):x} {uc.reg_read(A.UC_ARM_REG_LR):x} {pc:x} {xpsr & 0xF80F0000:x}"
        print(f"{line} {out}" + (" " + " ".join(changes) if changes else ""))
        written += 1


main()
