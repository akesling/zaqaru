//! What an instruction does to the values it reads, in a word: `add`,
//! `mul`, `lsl` — for saying how a value was made of others (a merge of
//! [`crate::provenance`]), not for running it. Thumb and Thumb-2, as
//! ARMv6-M and ARMv7-M have them; what does not make a value of others
//! (a branch, a compare, a load) is said as what it is, or `op`.

/// Whether the halfword `hw1` begins a 32-bit instruction.
pub fn wide(hw1: u32) -> bool {
    hw1 >> 11 >= 0b11101
}

/// The operation of the instruction `hw1` (and `hw2`, if [`wide`]).
pub fn operation(hw1: u32, hw2: u32) -> &'static str {
    if wide(hw1) { wide_operation(hw1, hw2) } else { narrow_operation(hw1) }
}

/// For a load or a store whose address is a register plus a register —
/// `ldr r0, [r1, r2, lsl #2]`, `tbh [r0, r1, lsl #1]` — how far left the
/// second is shifted; `None` for any other.
pub fn index_shift(hw1: u32, hw2: u32) -> Option<u32> {
    if !wide(hw1) {
        // LDR, STR and their kin, register offset.
        return (hw1 >> 12 == 0b0101).then_some(0);
    }
    // Thumb-2's loads and stores of one, register offset.
    if hw1 >> 9 == 0b1111100 && hw1 >> 7 & 1 == 0 && hw2 >> 6 & 0x3f == 0 {
        return Some(hw2 >> 4 & 3);
    }
    // TBB and TBH.
    if hw1 & 0xfff0 == 0xe8d0 && hw2 & 0xffe0 == 0xf000 {
        return Some(hw2 >> 4 & 1);
    }
    None
}

/// For Thumb-2's data processing with a shifted register for its second
/// operand — `add.w r4, r4, r0, lsr #26` — the shift and how far: `("lsr",
/// 26)`; `None` for any other, or for no shift.
pub fn shifted_operand(hw1: u32, hw2: u32) -> Option<(&'static str, u32)> {
    if !(wide(hw1) && hw1 >> 11 == 0b11101 && hw1 >> 9 & 3 == 0b01) || hw1 & 15 == 15 {
        return None;
    }
    let amount = (hw2 >> 12 & 7) << 2 | hw2 >> 6 & 3;
    match (hw2 >> 4 & 3, amount) {
        (0, 0) => None,
        (kind @ (1 | 2), 0) => Some((SHIFTS[kind as usize], 32)),
        (kind, n) => Some((SHIFTS[kind as usize], n)),
    }
}

/// The constant an instruction works with, besides the registers it
/// reads: what it adds, masks with, shifts by, or the bits it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Immediate {
    Value(u32),
    /// `ubfx`, `sbfx`, `bfi`, `bfc`: the bits `[lsb, lsb + width)`.
    Bits { lsb: u32, width: u32 },
}

/// The constant of the instruction `hw1` (and `hw2`), if it has one and
/// it goes into what the instruction makes — not an address's offset.
pub fn immediate(hw1: u32, hw2: u32) -> Option<Immediate> {
    use Immediate::*;
    if !wide(hw1) {
        let hw = hw1;
        return match hw >> 11 {
            // Shifts by an immediate: LSR and ASR encode 32 as 0.
            0b00000 => Some(Value(hw >> 6 & 31)),
            0b00001 | 0b00010 => Some(Value(match hw >> 6 & 31 {
                0 => 32,
                n => n,
            })),
            // ADD and SUB of three bits.
            0b00011 if hw >> 10 & 1 == 1 => Some(Value(hw >> 6 & 7)),
            // MOV, CMP, ADD, SUB of eight.
            0b00100..=0b00111 => Some(Value(hw & 0xff)),
            _ => None,
        };
    }
    match hw1 >> 11 {
        // A shifted register: a move's shift, by its amount.
        0b11101 if hw1 >> 9 & 3 == 0b01 && hw1 & 15 == 15 && hw1 >> 5 & 15 == 2 => {
            let amount = (hw2 >> 12 & 7) << 2 | hw2 >> 6 & 3;
            match (hw2 >> 4 & 3, amount) {
                (0, 0) => None,
                (1 | 2, 0) => Some(Value(32)),
                (_, n) => Some(Value(n)),
            }
        }
        0b11110 if hw2 >> 15 == 0 && hw1 >> 9 & 1 == 0 => {
            let imm12 = (hw1 >> 10 & 1) << 11 | (hw2 >> 12 & 7) << 8 | hw2 & 0xff;
            crate::thumb2::thumb_expand_imm_c(imm12, false).map(|(v, _)| Value(v))
        }
        0b11110 if hw2 >> 15 == 0 => {
            let imm12 = (hw1 >> 10 & 1) << 11 | (hw2 >> 12 & 7) << 8 | hw2 & 0xff;
            let lsb = (hw2 >> 12 & 7) << 2 | hw2 >> 6 & 3;
            match hw1 >> 4 & 31 {
                0b00000 | 0b01010 => Some(Value(imm12)),
                0b10100 | 0b11100 => Some(Bits { lsb, width: (hw2 & 31) + 1 }),
                0b10110 => Some(Bits { lsb, width: ((hw2 & 31) + 1).saturating_sub(lsb) }),
                _ => None,
            }
        }
        _ => None,
    }
}

const SHIFTS: [&str; 4] = ["lsl", "lsr", "asr", "ror"];

fn narrow_operation(hw: u32) -> &'static str {
    match hw >> 10 {
        // Shift by an immediate; add and subtract.
        0b000000..=0b000101 => SHIFTS[(hw >> 11 & 3) as usize],
        0b000110 | 0b000111 => {
            if hw >> 9 & 1 == 0 {
                "add"
            } else {
                "sub"
            }
        }
        // An immediate moved, compared, added, subtracted.
        0b001000..=0b001111 => ["mov", "cmp", "add", "sub"][(hw >> 11 & 3) as usize],
        0b010000 => ["and", "eor", "lsl", "lsr", "asr", "adc", "sbc", "ror", "tst", "neg", "cmp", "cmn", "orr", "mul", "bic", "mvn"][(hw >> 6 & 15) as usize],
        0b010001 => ["add", "cmp", "mov", "bx"][(hw >> 8 & 3) as usize],
        0b010010..=0b010011 => "ldr",
        0b010100..=0b100111 => {
            if hw >> 11 & 1 == 0 {
                "str"
            } else {
                "ldr"
            }
        }
        // An address made of the pc or the sp.
        0b101000..=0b101011 => "add",
        0b101100..=0b101111 => match hw >> 8 & 15 {
            0b0000 => {
                if hw >> 7 & 1 == 0 {
                    "add"
                } else {
                    "sub"
                }
            }
            0b0010 => ["sxth", "sxtb", "uxth", "uxtb"][(hw >> 6 & 3) as usize],
            0b1010 => ["rev", "rev16", "op", "revsh"][(hw >> 6 & 3) as usize],
            0b0100 | 0b0101 => "push",
            0b1100 | 0b1101 => "pop",
            _ => "op",
        },
        0b110000..=0b110001 => "stm",
        0b110010..=0b110011 => "ldm",
        _ => "op",
    }
}

/// The data-processing operations of Thumb-2's modified immediates and
/// shifted registers, by their `op` field.
const DATA: [&str; 16] = ["and", "bic", "orr", "orn", "eor", "op", "pkh", "op", "add", "op", "adc", "sbc", "op", "sub", "rsb", "op"];

fn wide_operation(hw1: u32, hw2: u32) -> &'static str {
    let rn = hw1 & 15;
    let rd = hw2 >> 8 & 15;
    match hw1 >> 11 {
        0b11101 => {
            if hw1 >> 9 & 3 == 0b01 {
                // Data processing, a shifted register.
                let op = hw1 >> 5 & 15;
                let flags = hw1 >> 4 & 1 == 1;
                match op {
                    0 if rd == 15 && flags => "tst",
                    4 if rd == 15 && flags => "teq",
                    8 if rd == 15 && flags => "cmn",
                    13 if rd == 15 && flags => "cmp",
                    // A move, shifted or not.
                    2 | 3 if rn == 15 => {
                        let shift = (hw2 >> 12 & 7) << 2 | hw2 >> 6 & 3;
                        if op == 3 {
                            "mvn"
                        } else if shift == 0 && hw2 >> 4 & 3 == 0 {
                            "mov"
                        } else {
                            SHIFTS[(hw2 >> 4 & 3) as usize]
                        }
                    }
                    op => DATA[op as usize],
                }
            } else if hw1 >> 9 & 3 == 0b00 {
                // Loads and stores of several, or of two.
                match (hw1 >> 6 & 1, hw1 >> 4 & 1) {
                    (0, 0) => "stm",
                    (0, _) => "ldm",
                    (_, 0) => "strd",
                    _ => "ldrd",
                }
            } else {
                "fpu"
            }
        }
        0b11110 if hw2 >> 15 == 0 => {
            if hw1 >> 9 & 1 == 0 {
                // A modified immediate.
                let op = hw1 >> 5 & 15;
                let flags = hw1 >> 4 & 1 == 1;
                match op {
                    0 if rd == 15 && flags => "tst",
                    4 if rd == 15 && flags => "teq",
                    8 if rd == 15 && flags => "cmn",
                    13 if rd == 15 && flags => "cmp",
                    2 if rn == 15 => "mov",
                    3 if rn == 15 => "mvn",
                    op => DATA[op as usize],
                }
            } else {
                // A plain immediate.
                match hw1 >> 4 & 31 {
                    0b00000 => "add",
                    0b00100 => "movw",
                    0b01010 => "sub",
                    0b01100 => "movt",
                    0b10000 | 0b10010 => "ssat",
                    0b10100 => "sbfx",
                    0b10110 => {
                        if rn == 15 {
                            "bfc"
                        } else {
                            "bfi"
                        }
                    }
                    0b11000 | 0b11010 => "usat",
                    0b11100 => "ubfx",
                    _ => "op",
                }
            }
        }
        0b11110 => "branch",
        _ => match hw1 >> 4 & 0x7f {
            // Loads and stores of one.
            0b0000000..=0b0011111 => {
                if hw1 >> 4 & 1 == 1 {
                    "ldr"
                } else {
                    "str"
                }
            }
            0b0100000..=0b0101111 => data_register(hw1, hw2),
            0b0110000..=0b0110111 => match (hw1 >> 4 & 7, hw2 >> 4 & 15) {
                (0, 0) if hw2 >> 12 & 15 == 15 => "mul",
                (0, 0) => "mla",
                (0, 1) => "mls",
                (_, _) if hw2 >> 12 & 15 == 15 => "mul",
                _ => "mla",
            },
            0b0111000..=0b0111111 => match (hw1 >> 4 & 7, hw2 >> 4 & 15) {
                (0b000, _) => "smull",
                (0b001, 0b1111) => "sdiv",
                (0b010, _) => "umull",
                (0b011, 0b1111) => "udiv",
                (0b100, _) => "smlal",
                (0b110, _) => "umlal",
                _ => "mul",
            },
            _ => "fpu",
        },
    }
}

/// Thumb-2's data processing on registers: shifts by a register, extends,
/// the parallel arithmetic, and the miscellaneous (`rev`, `clz`).
fn data_register(hw1: u32, hw2: u32) -> &'static str {
    let op1 = hw1 >> 4 & 15;
    let op2 = hw2 >> 4 & 15;
    if op2 == 0 && op1 < 8 {
        return SHIFTS[(op1 >> 1) as usize];
    }
    if op1 < 6 && op2 >> 3 == 1 {
        let extend = ["sxth", "uxth", "sxtb16", "uxtb16", "sxtb", "uxtb"][op1 as usize];
        // With a register added: `sxtah` and the like, an add.
        return if hw1 & 15 == 15 { extend } else { "add" };
    }
    if op1 >> 2 == 0b10 && op2 >> 2 == 0b00 {
        return ["qadd", "qdadd", "qsub", "qdsub"][(op2 & 3) as usize];
    }
    if op1 >> 2 == 0b10 && op2 >> 2 == 0b10 {
        return match (op1 & 3, op2 & 3) {
            (0b01, 0) => "rev",
            (0b01, 1) => "rev16",
            (0b01, 2) => "rbit",
            (0b01, 3) => "revsh",
            (0b10, 0) => "sel",
            (0b11, 0) => "clz",
            _ => "op",
        };
    }
    // Parallel addition and subtraction, a lane at a time.
    if op1 & 4 == 0 { "add" } else { "sub" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(halfwords: &[u16]) -> &'static str {
        operation(u32::from(halfwords[0]), u32::from(*halfwords.get(1).unwrap_or(&0)))
    }

    #[test]
    fn instructions_are_named_by_what_they_do_to_values() {
        assert_eq!(named(&[0x1888]), "add"); // adds r0, r1, r2
        assert_eq!(named(&[0x1a88]), "sub"); // subs r0, r1, r2
        assert_eq!(named(&[0x0108]), "lsl"); // lsls r0, r1, #4
        assert_eq!(named(&[0x4348]), "mul"); // muls r0, r1, r0
        assert_eq!(named(&[0x4308]), "orr"); // orrs r0, r1
        assert_eq!(named(&[0x44c8]), "add"); // add r8, r9
        assert_eq!(named(&[0xb2c8]), "uxtb"); // uxtb r0, r1
        assert_eq!(named(&[0xeb01, 0x0002]), "add"); // add.w r0, r1, r2
        assert_eq!(named(&[0xea47, 0x2606]), "orr"); // orr.w r6, r7, r6, lsl #8
        assert_eq!(named(&[0xea4f, 0x1021]), "asr"); // asr.w r0, r1, #4
        assert_eq!(named(&[0xfb01, 0xf002]), "mul"); // mul r0, r1, r2
        assert_eq!(named(&[0xfb01, 0x3002]), "mla"); // mla r0, r1, r2, r3
        assert_eq!(named(&[0xfb91, 0xf0f2]), "sdiv"); // sdiv r0, r1, r2
        assert_eq!(named(&[0xfb81, 0x0102]), "smull"); // smull r0, r1, r1, r2
        assert_eq!(named(&[0xfa01, 0xf002]), "lsl"); // lsl.w r0, r1, r2
        assert_eq!(named(&[0xf3c1, 0x1003]), "ubfx"); // ubfx r0, r1, #4, #4
        assert_eq!(named(&[0xf101, 0x0001]), "add"); // add.w r0, r1, #1
        assert_eq!(named(&[0xf2c1, 0x2034]), "movt"); // movt r0, #0x1234
        assert_eq!(named(&[0xfab1, 0xf081]), "clz"); // clz r0, r1
    }

    #[test]
    fn an_instruction_s_constant_is_read_out_of_it() {
        use Immediate::*;
        let imm = |h: &[u16]| immediate(u32::from(h[0]), u32::from(*h.get(1).unwrap_or(&0)));
        assert_eq!(imm(&[0x0108]), Some(Value(4))); // lsls r0, r1, #4
        assert_eq!(imm(&[0x15d2]), Some(Value(23))); // asrs r2, r2, #0x17
        assert_eq!(imm(&[0x3001]), Some(Value(1))); // adds r0, #1
        assert_eq!(imm(&[0x1c48]), Some(Value(1))); // adds r0, r1, #1
        assert_eq!(imm(&[0x1888]), None); // adds r0, r1, r2
        assert_eq!(imm(&[0xea4f, 0x1021]), Some(Value(4))); // asr.w r0, r1, #4
        assert_eq!(imm(&[0xf101, 0x0001]), Some(Value(1))); // add.w r0, r1, #1
        assert_eq!(imm(&[0xf3c1, 0x1003]), Some(Bits { lsb: 4, width: 4 })); // ubfx r0, r1, #4, #4
        assert_eq!(imm(&[0xf401, 0x607c]), Some(Value(0xfc0))); // and r0, r1, #0xfc0
    }

    #[test]
    fn a_second_operand_s_shift_is_read_out_of_it() {
        assert_eq!(shifted_operand(0xeb04, 0x6490), Some(("lsr", 26))); // add.w r4, r4, r0, lsr #26
        assert_eq!(shifted_operand(0xea47, 0x2606), Some(("lsl", 8))); // orr.w r6, r7, r6, lsl #8
        assert_eq!(shifted_operand(0xeb01, 0x0002), None); // add.w r0, r1, r2
        assert_eq!(shifted_operand(0xea4f, 0x1021), None); // asr.w r0, r1, #4: a move
    }

    #[test]
    fn an_address_of_two_registers_says_how_far_the_index_is_shifted() {
        assert_eq!(index_shift(0xf855, 0x002a), Some(2)); // ldr.w r0, [r5, r10, lsl #2]
        assert_eq!(index_shift(0xf840, 0x1022), Some(2)); // str.w r1, [r0, r2, lsl #2]
        assert_eq!(index_shift(0x5c8a, 0), Some(0)); // ldrb r2, [r1, r2]
        assert_eq!(index_shift(0xe8d0, 0xf011), Some(1)); // tbh [r0, r1, lsl #1]
        assert_eq!(index_shift(0x6900, 0), None); // ldr r0, [r0, #0x10]
        assert_eq!(index_shift(0xf8d3, 0xe000), None); // ldr.w lr, [r3]
    }
}
