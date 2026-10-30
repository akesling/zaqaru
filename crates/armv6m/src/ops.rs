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
}
