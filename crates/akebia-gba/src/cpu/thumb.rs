//! Decoding and running an instruction in THUMB state.
//!
//! # What THUMB is and why every game is written in it
//!
//! The same processor, driven through a second instruction set half the width.
//! Sixteen bits instead of thirty-two, which halves the size of a program — and
//! on this machine the cartridge sits on a sixteen-bit bus, so a program in
//! THUMB is not only smaller but **fetched in one access where an ARM
//! instruction takes two**. It is faster despite doing less per instruction.
//!
//! That is why a game is almost entirely THUMB and its ARM is a few dozen
//! instructions of startup. An emulator with only the wider set runs the first
//! thirteen instructions of a cartridge and stops.
//!
//! # What was given up to fit
//!
//! **The condition field.** Only the branch keeps one; everything else always
//! happens, and an `if` costs a real branch again.
//!
//! **Half the registers, most of the time.** Three bits name a register, so
//! most instructions reach `R0`-`R7` only. `R8`-`R12` are reachable through one
//! instruction that spends a bit on saying so, and the stack pointer, the link
//! register and the counter get instructions of their own.
//!
//! **The choice of whether to set flags.** Almost every data instruction sets
//! them, whether or not anything is going to read them. The exceptions are the
//! high-register forms, which set none — and that asymmetry is not tidy-able,
//! it is what the encodings say.
//!
//! # What is the same underneath
//!
//! Everything below the decoding. The same shifter, the same adder, the same
//! flag rules, the same register file — a THUMB instruction is a short way of
//! writing something the ARM set can already say. So this module is nearly all
//! decode, handing the work to [`super::alu`] and [`super::shift`], and where
//! it does something those cannot express that is worth remarking on.

use super::alu::{self, Op};
use super::condition::Condition;
use super::registers::Registers;
use super::shift::{self, Kind};
use super::{Bus, Fault};

/// How far ahead `R15` reads while a THUMB instruction executes: one
/// instruction of the pair in flight, so four bytes rather than the ARM set's
/// eight.
const PIPELINE: u32 = 4;

/// Runs one THUMB instruction, which has already been fetched.
pub fn execute(
    regs: &mut Registers,
    bus: &mut impl Bus,
    addr: u32,
    instruction: u16,
) -> Result<(), Fault> {
    let top = instruction >> 12;
    match top {
        0b0000 | 0b0001 => {
            if instruction & 0x1800 == 0x1800 {
                add_or_subtract(regs, instruction);
            } else {
                shift_by_constant(regs, instruction);
            }
            Ok(())
        }
        0b0010 | 0b0011 => {
            immediate_operation(regs, instruction);
            Ok(())
        }
        0b0100 => {
            if instruction & 0x0C00 == 0x0000 {
                alu_operation(regs, instruction);
                Ok(())
            } else if instruction & 0x0C00 == 0x0400 {
                high_register_operation(regs, addr, instruction);
                Ok(())
            } else {
                // A load of a constant from just after the code, and the only
                // instruction that reaches the counter to do it.
                counter_relative_load(regs, bus, addr, instruction);
                Ok(())
            }
        }
        0b0101 => {
            register_offset_transfer(regs, bus, instruction);
            Ok(())
        }
        0b0110 | 0b0111 => {
            constant_offset_transfer(regs, bus, instruction);
            Ok(())
        }
        0b1000 => {
            halfword_transfer(regs, bus, instruction);
            Ok(())
        }
        0b1001 => {
            stack_relative_transfer(regs, bus, instruction);
            Ok(())
        }
        0b1010 => {
            load_address(regs, addr, instruction);
            Ok(())
        }
        0b1011 => miscellaneous(regs, bus, addr, instruction),
        0b1100 => {
            block_transfer(regs, bus, instruction);
            Ok(())
        }
        // The condition field of a branch has two values that are not
        // conditions. One is the call into the BIOS; the other is reserved and
        // decodes to nothing, and must not be allowed to fall through as the
        // "never" it looks like.
        0b1101 if instruction & 0xFF00 == 0xDF00 => {
            super::arm::software_interrupt(regs, addr.wrapping_add(2));
            Ok(())
        }
        0b1101 if instruction & 0xFF00 == 0xDE00 => {
            Err(Fault::Undefined { addr, instruction: u32::from(instruction) })
        }
        0b1101 => {
            conditional_branch(regs, addr, instruction);
            Ok(())
        }
        0b1110 if instruction & 0x0800 == 0 => {
            branch(regs, addr, instruction);
            Ok(())
        }
        0b1111 => {
            long_branch(regs, addr, instruction);
            Ok(())
        }
        _ => Err(Fault::Undefined { addr, instruction: u32::from(instruction) }),
    }
}

/// Puts an outcome's flags away, leaving the overflow alone when the operation
/// had nothing to say about it.
fn write_flags(regs: &mut Registers, out: &alu::Outcome) {
    regs.set_nz(out.result);
    regs.set_c(out.carry);
    if let Some(overflow) = out.overflow {
        regs.set_v(overflow);
    }
}

/// The three registers most instructions name, each three bits wide.
fn low(instruction: u16, at: u32) -> usize {
    ((instruction >> at) & 0b111) as usize
}

/// Format 1: a shift by a constant, which is how `MOV` between low registers is
/// written — there is no plain move, only a shift by nothing.
fn shift_by_constant(regs: &mut Registers, instruction: u16) {
    let kind = Kind::from_bits(u32::from(instruction >> 11));
    let amount = u32::from((instruction >> 6) & 0x1F);
    let value = regs.get(low(instruction, 3));

    let out = shift::by_immediate(kind, amount, value, regs.c());
    regs.set(low(instruction, 0), out.value);
    regs.set_nz(out.value);
    regs.set_c(out.carry);
}

/// Format 2: add or subtract a register or a three-bit constant.
fn add_or_subtract(regs: &mut Registers, instruction: u16) {
    let operand = if instruction & 0x0400 != 0 {
        u32::from((instruction >> 6) & 0b111)
    } else {
        regs.get(low(instruction, 6))
    };
    let op = if instruction & 0x0200 != 0 { Op::Sub } else { Op::Add };
    let out = alu::execute(op, regs.get(low(instruction, 3)), operand, regs.c(), regs.c());

    regs.set(low(instruction, 0), out.result);
    write_flags(regs, &out);
}

/// Format 3: one register against an eight-bit constant, which is the widest
/// constant THUMB can name.
fn immediate_operation(regs: &mut Registers, instruction: u16) {
    let op = match (instruction >> 11) & 0b11 {
        0b00 => Op::Mov,
        0b01 => Op::Cmp,
        0b10 => Op::Add,
        _ => Op::Sub,
    };
    let rd = low(instruction, 8);
    let out = alu::execute(op, regs.get(rd), u32::from(instruction & 0xFF), regs.c(), regs.c());

    if op.writes_result() {
        regs.set(rd, out.result);
    }
    write_flags(regs, &out);
}

/// Format 4: the sixteen operations between two low registers.
///
/// It is not the same sixteen as the ARM set's, and the differences are the
/// interesting part: the shifts are here as operations in their own right
/// rather than as part of an operand, there is a negate that the wider set
/// writes as a reverse-subtract from nothing, and there is a multiply.
fn alu_operation(regs: &mut Registers, instruction: u16) {
    let rd = low(instruction, 0);
    let rs = low(instruction, 3);
    let (first, second) = (regs.get(rd), regs.get(rs));

    let out = match (instruction >> 6) & 0xF {
        0x0 => alu::execute(Op::And, first, second, regs.c(), regs.c()),
        0x1 => alu::execute(Op::Eor, first, second, regs.c(), regs.c()),
        // The shifts take their amount from a register, so a zero in it means
        // no shift rather than one of the constant form's special encodings.
        0x2 => shifted(Kind::Lsl, first, second, regs.c()),
        0x3 => shifted(Kind::Lsr, first, second, regs.c()),
        0x4 => shifted(Kind::Asr, first, second, regs.c()),
        0x5 => alu::execute(Op::Adc, first, second, regs.c(), regs.c()),
        0x6 => alu::execute(Op::Sbc, first, second, regs.c(), regs.c()),
        0x7 => shifted(Kind::Ror, first, second, regs.c()),
        0x8 => alu::execute(Op::Tst, first, second, regs.c(), regs.c()),
        // Negate: nothing less the operand, which is the reverse-subtract the
        // wider set would write.
        0x9 => alu::execute(Op::Rsb, second, 0, regs.c(), regs.c()),
        0xA => alu::execute(Op::Cmp, first, second, regs.c(), regs.c()),
        0xB => alu::execute(Op::Cmn, first, second, regs.c(), regs.c()),
        0xC => alu::execute(Op::Orr, first, second, regs.c(), regs.c()),
        0xD => {
            // A multiply, which sets the two flags a multiply sets and leaves
            // the carry alone for the reason the wider set does.
            let result = first.wrapping_mul(second);
            regs.set(rd, result);
            regs.set_nz(result);
            return;
        }
        0xE => alu::execute(Op::Bic, first, second, regs.c(), regs.c()),
        _ => alu::execute(Op::Mvn, first, second, regs.c(), regs.c()),
    };

    // `TST`, `CMP` and `CMN` are the three that keep nothing. The negate writes
    // to `Rd` like the rest, its operand having come from `Rs`.
    let keeps = !matches!((instruction >> 6) & 0xF, 0x8 | 0xA | 0xB);
    if keeps {
        regs.set(rd, out.result);
    }
    write_flags(regs, &out);
}

/// A shift standing on its own as an operation, rather than folded into an
/// operand as the wider set folds it.
fn shifted(kind: Kind, value: u32, amount: u32, carry: bool) -> alu::Outcome {
    let out = shift::by_register(kind, amount, value, carry);
    alu::Outcome { result: out.value, carry: out.carry, overflow: None }
}

/// Format 5: the three operations that can reach the high registers, and the
/// branch that changes instruction set.
///
/// These set **no flags at all**, which is the one asymmetry in THUMB that
/// cannot be tidied away — `ADD` and `MOV` here are the only data instructions
/// in the set that leave the flags alone, and `CMP` is the only one of the
/// three that touches them, because comparing is all it does.
fn high_register_operation(regs: &mut Registers, addr: u32, instruction: u16) {
    let rd = low(instruction, 0) | ((instruction as usize >> 4) & 0b1000);
    let rs = low(instruction, 3) | ((instruction as usize >> 3) & 0b1000);
    let pc = addr.wrapping_add(PIPELINE);
    let read = |regs: &Registers, index: usize| if index == 15 { pc } else { regs.get(index) };

    match (instruction >> 8) & 0b11 {
        0b00 => {
            let sum = read(regs, rd).wrapping_add(read(regs, rs));
            write_maybe_counter(regs, rd, sum);
        }
        0b01 => {
            let out =
                alu::execute(Op::Cmp, read(regs, rd), read(regs, rs), regs.c(), regs.c());
            write_flags(regs, &out);
        }
        0b10 => {
            let value = read(regs, rs);
            write_maybe_counter(regs, rd, value);
        }
        // `BX`, reached from here because there was nowhere else to put it. The
        // bottom bit of the target chooses the instruction set, exactly as in
        // the wider set, and this is how a THUMB program calls an ARM one.
        _ => {
            let target = read(regs, rs);
            let thumb = target & 1 != 0;
            regs.set_thumb(thumb);
            regs.set_pc(if thumb { target & !1 } else { target & !3 });
        }
    }
}

/// Writing a register that might be the counter, in which case it is a branch
/// and the address has to be aligned.
fn write_maybe_counter(regs: &mut Registers, index: usize, value: u32) {
    if index == 15 {
        regs.set_pc(value & !1);
    } else {
        regs.set(index, value);
    }
}

/// Format 6: a constant fetched from just past the code.
///
/// THUMB can only name eight bits of constant directly, so anything wider is
/// assembled into a pool after the function and loaded from there. This is the
/// instruction that does it, and the counter it counts from is **rounded down
/// to a word** — the instruction itself may sit at a halfword, and the pool
/// never does.
fn counter_relative_load(regs: &mut Registers, bus: &mut impl Bus, addr: u32, instruction: u16) {
    let base = addr.wrapping_add(PIPELINE) & !3;
    let at = base.wrapping_add(u32::from(instruction & 0xFF) * 4);
    let value = bus.read32(at);
    regs.set(low(instruction, 8), value);
}

/// Format 7 and 8: a transfer whose offset is a register.
fn register_offset_transfer(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let rd = low(instruction, 0);
    let at = regs.get(low(instruction, 3)).wrapping_add(regs.get(low(instruction, 6)));

    // Bit 9 divides the two formats: without it the plain word and byte, with
    // it the halfword and the two that widen by sign.
    if instruction & 0x0200 == 0 {
        match (instruction >> 10) & 0b11 {
            0b00 => bus.write32(at, regs.get(rd)),
            0b01 => bus.write8(at, regs.get(rd) as u8),
            0b10 => {
                let value = bus.read32(at).rotate_right((at & 3) * 8);
                regs.set(rd, value);
            }
            _ => {
                let value = u32::from(bus.read8(at));
                regs.set(rd, value);
            }
        }
        return;
    }

    match (instruction >> 10) & 0b11 {
        0b00 => bus.write16(at, regs.get(rd) as u16),
        0b01 => {
            let value = bus.read8(at) as i8 as u32;
            regs.set(rd, value);
        }
        0b10 => {
            let value = u32::from(bus.read16(at)).rotate_right((at & 1) * 8);
            regs.set(rd, value);
        }
        // A signed halfword, with the same giving-up from an odd address that
        // the wider set has.
        _ => {
            let value = if at & 1 != 0 {
                bus.read8(at) as i8 as u32
            } else {
                bus.read16(at) as i16 as u32
            };
            regs.set(rd, value);
        }
    }
}

/// Format 9: a word or a byte at a constant offset.
///
/// The offset is scaled by the width, which is what lets five bits reach a
/// hundred and twenty-four bytes into a structure rather than thirty-one.
fn constant_offset_transfer(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let rd = low(instruction, 0);
    let byte = instruction & 0x1000 != 0;
    let offset = u32::from((instruction >> 6) & 0x1F) * if byte { 1 } else { 4 };
    let at = regs.get(low(instruction, 3)).wrapping_add(offset);

    match (instruction & 0x0800 != 0, byte) {
        (false, false) => bus.write32(at, regs.get(rd)),
        (false, true) => bus.write8(at, regs.get(rd) as u8),
        (true, false) => {
            let value = bus.read32(at).rotate_right((at & 3) * 8);
            regs.set(rd, value);
        }
        (true, true) => {
            let value = u32::from(bus.read8(at));
            regs.set(rd, value);
        }
    }
}

/// Format 10: a halfword at a constant offset, scaled by two.
fn halfword_transfer(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let rd = low(instruction, 0);
    let at = regs.get(low(instruction, 3)).wrapping_add(u32::from((instruction >> 6) & 0x1F) * 2);

    if instruction & 0x0800 != 0 {
        let value = u32::from(bus.read16(at)).rotate_right((at & 1) * 8);
        regs.set(rd, value);
    } else {
        bus.write16(at, regs.get(rd) as u16);
    }
}

/// Format 11: a transfer relative to the stack pointer, which is where a
/// function's own variables live.
fn stack_relative_transfer(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let rd = low(instruction, 8);
    let at = regs.sp().wrapping_add(u32::from(instruction & 0xFF) * 4);

    if instruction & 0x0800 != 0 {
        let value = bus.read32(at).rotate_right((at & 3) * 8);
        regs.set(rd, value);
    } else {
        bus.write32(at, regs.get(rd));
    }
}

/// Format 12: the address of something, rather than the thing.
///
/// From the counter it is how the address of a constant pool entry is taken;
/// from the stack pointer it is how the address of a local variable is. The
/// counter is rounded down to a word here for the same reason as in format 6.
fn load_address(regs: &mut Registers, addr: u32, instruction: u16) {
    let base = if instruction & 0x0800 != 0 { regs.sp() } else { addr.wrapping_add(PIPELINE) & !3 };
    let value = base.wrapping_add(u32::from(instruction & 0xFF) * 4);
    regs.set(low(instruction, 8), value);
}

/// The corner that holds the stack instructions and nothing else.
fn miscellaneous(
    regs: &mut Registers,
    bus: &mut impl Bus,
    addr: u32,
    instruction: u16,
) -> Result<(), Fault> {
    if instruction & 0xFF00 == 0xB000 {
        // Format 13: move the stack pointer by a constant, which is a function
        // making room for its own variables. The sign is a bit rather than a
        // negative number, the constant being unsigned.
        let offset = u32::from(instruction & 0x7F) * 4;
        let sp = regs.sp();
        regs.set(13, if instruction & 0x0080 != 0 { sp.wrapping_sub(offset) } else { sp.wrapping_add(offset) });
        return Ok(());
    }

    if instruction & 0xF600 == 0xB400 {
        push_or_pop(regs, bus, instruction);
        return Ok(());
    }

    Err(Fault::Undefined { addr, instruction: u32::from(instruction) })
}

/// Format 14: `PUSH` and `POP`.
///
/// The whole of a function's entry and exit. The extra bit is what makes it so:
/// on a push it adds the link register, and on a pop it adds the *counter*, so
/// that a function returns by popping straight into it. One instruction to come
/// back, and the register that would have held the address never has to be
/// spilled.
fn push_or_pop(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let popping = instruction & 0x0800 != 0;
    let extra = instruction & 0x0100 != 0;
    let mut list = u32::from(instruction & 0xFF);
    if extra {
        list |= if popping { 1 << 15 } else { 1 << 14 };
    }

    let span = list.count_ones() * 4;

    if popping {
        // Upward from where the pointer stands, in ascending register order,
        // which is the same rule the wider set's block transfers follow.
        let mut at = regs.sp();
        for index in 0..16usize {
            if list & (1 << index) == 0 {
                continue;
            }
            let value = bus.read32(at);
            if index == 15 {
                // Returning. On this processor the bottom bit is not read as an
                // instruction-set switch here, the popped address simply being
                // aligned — a THUMB function returns to THUMB.
                regs.set_pc(value & !1);
            } else {
                regs.set(index, value);
            }
            at = at.wrapping_add(4);
        }
        regs.set(13, regs.sp().wrapping_add(span));
    } else {
        let bottom = regs.sp().wrapping_sub(span);
        let mut at = bottom;
        for index in 0..16usize {
            if list & (1 << index) == 0 {
                continue;
            }
            bus.write32(at, regs.get(index));
            at = at.wrapping_add(4);
        }
        regs.set(13, bottom);
    }
}

/// Format 15: a block transfer through a low register, which is how a run of
/// words is copied.
fn block_transfer(regs: &mut Registers, bus: &mut impl Bus, instruction: u16) {
    let rb = low(instruction, 8);
    let list = u32::from(instruction & 0xFF);
    let mut at = regs.get(rb);

    // An empty list has the same strangeness here as in the wider set: the
    // counter alone is transferred and the base moves by sixteen registers'
    // worth.
    if list == 0 {
        if instruction & 0x0800 != 0 {
            let value = bus.read32(at);
            regs.set_pc(value & !1);
        } else {
            bus.write32(at, regs.pc().wrapping_add(PIPELINE));
        }
        regs.set(rb, at.wrapping_add(0x40));
        return;
    }

    let moved = at.wrapping_add(list.count_ones() * 4);
    let loading = instruction & 0x0800 != 0;
    let mut first = true;

    for index in 0..8usize {
        if list & (1 << index) == 0 {
            continue;
        }
        if loading {
            let value = bus.read32(at);
            regs.set(index, value);
        } else {
            // The same rule as the wider set: the base goes down moved unless
            // it is the first one out.
            let value = if index == rb && !first { moved } else { regs.get(index) };
            bus.write32(at, value);
        }
        at = at.wrapping_add(4);
        first = false;
    }

    // A load that brought a new value into the base keeps it.
    if !(loading && list & (1 << rb) != 0) {
        regs.set(rb, moved);
    }
}

/// Format 16: the only conditional instruction left in the set.
fn conditional_branch(regs: &mut Registers, addr: u32, instruction: u16) {
    if !Condition::from_bits(u32::from((instruction >> 8) & 0xF)).passes(regs) {
        return;
    }
    // Eight bits of signed offset, scaled to halfwords: a reach of about two
    // hundred and fifty bytes each way, which is what an `if` inside a function
    // needs and no more.
    let offset = ((instruction & 0xFF) as u8 as i8 as i32) * 2;
    regs.set_pc(addr.wrapping_add(PIPELINE).wrapping_add(offset as u32));
}

/// Format 18: a branch with no condition, reaching further.
fn branch(regs: &mut Registers, addr: u32, instruction: u16) {
    let offset = ((instruction & 0x07FF) << 5) as i16 as i32 >> 4;
    regs.set_pc(addr.wrapping_add(PIPELINE).wrapping_add(offset as u32));
}

/// Format 19: a call, in two instructions.
///
/// # Why it takes two
///
/// Because a call has to reach the whole address space and sixteen bits cannot
/// hold that offset. So it is split: the first instruction puts the top half of
/// the offset into the link register, and the second adds the bottom half and
/// swaps what is left there for the way back. They are separate instructions in
/// every sense — an interrupt can land between them, and the pair still works,
/// because everything either of them needs is in the link register rather than
/// in any state of the processor's.
fn long_branch(regs: &mut Registers, addr: u32, instruction: u16) {
    let offset = u32::from(instruction & 0x07FF);

    if instruction & 0x0800 == 0 {
        // The top eleven bits, sign-extended from the whole twenty-three.
        let high = ((offset << 21) as i32 >> 9) as u32;
        regs.set(14, addr.wrapping_add(PIPELINE).wrapping_add(high));
        return;
    }

    let target = regs.lr().wrapping_add(offset * 2);
    // The way back is the instruction after this one, with its bottom bit set
    // so that returning through `BX` comes back into THUMB.
    regs.set(14, addr.wrapping_add(2) | 1);
    regs.set_pc(target & !1);
}

#[cfg(test)]
mod tests {
    use crate::bus::Memory;
    use crate::cpu::{Bus, Cpu, Mode};

    const BASE: u32 = 0x0300_0000;
    const DATA: u32 = BASE + 0x400;

    /// A processor in THUMB state with a program at [`BASE`].
    fn machine(program: &[u16]) -> (Cpu, Memory) {
        let mut mem = Memory::new();
        for (index, halfword) in program.iter().enumerate() {
            mem.write16(BASE + index as u32 * 2, *halfword);
        }
        let mut cpu = Cpu::new();
        cpu.regs.set_mode(Mode::System);
        cpu.regs.set_thumb(true);
        cpu.regs.set_pc(BASE);
        (cpu, mem)
    }

    fn run(program: &[u16]) -> (Cpu, Memory) {
        let (mut cpu, mut mem) = machine(program);
        for _ in 0..program.len() {
            cpu.step(&mut mem).expect("the program faulted");
        }
        (cpu, mem)
    }

    /// The counter walks by two, not four.
    #[test]
    fn the_counter_walks_on_by_two() {
        let (cpu, _) = run(&[0x2001, 0x2002, 0x2003]);
        assert_eq!(cpu.regs.pc(), BASE + 6);
        assert_eq!(cpu.regs.get(0), 3, "and each one happened");
    }

    /// There is no plain move between low registers: it is written as a shift
    /// by nothing, which is why the shift is format one.
    #[test]
    fn a_move_between_low_registers_is_a_shift_by_nothing() {
        // MOV r0, r1  ->  LSL r0, r1, #0
        let (mut cpu, mut mem) = machine(&[0x0008]);
        cpu.regs.set(1, 0x1234);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x1234);
    }

    #[test]
    fn a_shift_by_a_constant_sets_the_sign_the_zero_and_the_carry() {
        // LSL r0, r1, #4
        let (mut cpu, mut mem) = machine(&[0x0108]);
        cpu.regs.set(1, 0x0800_0001);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x8000_0010);
        assert!(cpu.regs.n(), "the answer is negative");
        assert!(!cpu.regs.c(), "and nothing fell off the top");

        // LSR r0, r1, #1, with the bit that falls off going to the carry.
        let (mut cpu, mut mem) = machine(&[0x0848]);
        cpu.regs.set(1, 3);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 1);
        assert!(cpu.regs.c());
    }

    #[test]
    fn adding_and_subtracting_take_a_register_or_a_small_constant() {
        // ADD r0, r1, r2
        let (mut cpu, mut mem) = machine(&[0x1888]);
        cpu.regs.set(1, 10);
        cpu.regs.set(2, 5);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 15);

        // SUB r0, r1, #3
        let (mut cpu, mut mem) = machine(&[0x1EC8]);
        cpu.regs.set(1, 10);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 7);
        assert!(cpu.regs.c(), "and it did not borrow");
    }

    /// Eight bits is the widest constant THUMB can name outright.
    #[test]
    fn a_register_against_an_eight_bit_constant() {
        // MOV r0,#255 / CMP r0,#255 / ADD r0,#1 / SUB r0,#2
        let (cpu, _) = run(&[0x20FF, 0x28FF, 0x3001, 0x3802]);
        assert_eq!(cpu.regs.get(0), 254);
    }

    #[test]
    fn a_comparison_against_a_constant_writes_flags_and_no_register() {
        // CMP r0, #5
        let (mut cpu, mut mem) = machine(&[0x2805]);
        cpu.regs.set(0, 5);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 5, "the register is untouched");
        assert!(cpu.regs.z(), "and the flags moved");
    }

    /// The sixteen operations between low registers, which are not the same
    /// sixteen the wider set has.
    #[test]
    fn the_alu_operations_include_a_negate_and_a_multiply_the_wider_set_spells_out() {
        // NEG r0, r1
        let (mut cpu, mut mem) = machine(&[0x4248]);
        cpu.regs.set(1, 5);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0xFFFF_FFFB, "nothing less five");
        assert!(cpu.regs.n());

        // MUL r0, r1
        let (mut cpu, mut mem) = machine(&[0x4348]);
        cpu.regs.set(0, 6);
        cpu.regs.set(1, 7);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 42);
    }

    /// The shifts appear again here as operations in their own right, taking
    /// their amount from a register - where a zero means no shift rather than
    /// one of the constant form's special encodings.
    #[test]
    fn a_shift_by_a_register_is_its_own_operation_with_its_own_rules() {
        // LSR r0, r1 with r1 holding zero: nothing happens, carry included.
        let (mut cpu, mut mem) = machine(&[0x40C8]);
        cpu.regs.set(0, 0x8000_0000);
        cpu.regs.set(1, 0);
        cpu.regs.set_c(true);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x8000_0000, "unshifted");
        assert!(cpu.regs.c(), "and the carry untouched");

        // And by thirty-two, where everything goes and the top bit is the last
        // to fall off.
        let (mut cpu, mut mem) = machine(&[0x40C8]);
        cpu.regs.set(0, 0x8000_0000);
        cpu.regs.set(1, 32);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0);
        assert!(cpu.regs.c());
    }

    #[test]
    fn the_three_comparisons_keep_nothing() {
        // TST r0, r1
        let (mut cpu, mut mem) = machine(&[0x4208]);
        cpu.regs.set(0, 0xFF00);
        cpu.regs.set(1, 0x00FF);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0xFF00, "untouched");
        assert!(cpu.regs.z(), "and nothing overlapped");
    }

    /// The high-register forms are the only data instructions in the set that
    /// set no flags, and `CMP` among them is the only one that does.
    #[test]
    fn the_high_register_forms_set_no_flags_except_the_comparison() {
        // ADD r0, r8
        let (mut cpu, mut mem) = machine(&[0x4440]);
        cpu.regs.set(0, 1);
        cpu.regs.set(8, 0xFFFF_FFFF);
        cpu.regs.set_z(false);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0, "it wrapped to zero");
        assert!(!cpu.regs.z(), "and said nothing about it");

        // CMP r0, r8 does say something.
        let (mut cpu, mut mem) = machine(&[0x4540]);
        cpu.regs.set(0, 5);
        cpu.regs.set(8, 5);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.z());
    }

    /// And they are the only way to reach r8 through r12 at all.
    #[test]
    fn the_high_registers_are_reachable_only_through_that_one_form() {
        // MOV r9, r1
        let (mut cpu, mut mem) = machine(&[0x4689]);
        cpu.regs.set(1, 0xABCD);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(9), 0xABCD);
    }

    /// `BX` from THUMB is how a program calls into ARM code.
    #[test]
    fn branch_and_exchange_goes_the_other_way_too() {
        // BX r1, with an even target: out of THUMB.
        let (mut cpu, mut mem) = machine(&[0x4708]);
        cpu.regs.set(1, BASE + 0x40);
        cpu.step(&mut mem).unwrap();
        assert!(!cpu.regs.thumb(), "into ARM");
        assert_eq!(cpu.regs.pc(), BASE + 0x40);

        // And with the bottom bit set it stays.
        let (mut cpu, mut mem) = machine(&[0x4708]);
        cpu.regs.set(1, BASE + 0x41);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.thumb());
        assert_eq!(cpu.regs.pc(), BASE + 0x40);
    }

    /// A constant wider than eight bits is fetched from a pool after the code,
    /// and the counter it counts from is rounded down to a word - the
    /// instruction may sit at a halfword and the pool never does.
    #[test]
    fn a_wide_constant_comes_from_a_pool_counted_from_a_rounded_counter() {
        // At BASE: LDR r0,[pc,#0]. PC+4 is BASE+4, already aligned, so the pool
        // entry is at BASE+4.
        let (mut cpu, mut mem) = machine(&[0x4800, 0x0000]);
        mem.write32(BASE + 4, 0x1234_5678);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x1234_5678);

        // The same instruction at an odd halfword: PC+4 is BASE+6, which rounds
        // down to BASE+4, so it reads the *same* word. That rounding is the
        // whole point of this test.
        let (mut cpu, mut mem) = machine(&[0x0000, 0x4800]);
        mem.write32(BASE + 4, 0xCAFE_BABE);
        cpu.regs.set_pc(BASE + 2);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0xCAFE_BABE, "rounded down to the word");
    }

    #[test]
    fn a_transfer_can_take_its_offset_from_a_register() {
        // STR r0,[r1,r2] / LDR r3,[r1,r2]
        let (mut cpu, mut mem) = machine(&[0x5088, 0x588B]);
        cpu.regs.set(0, 0x1234_5678);
        cpu.regs.set(1, DATA);
        cpu.regs.set(2, 8);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA + 8), 0x1234_5678);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(3), 0x1234_5678);
    }

    /// The constant offset is scaled by the width, which is what lets five bits
    /// reach a hundred and twenty-four bytes into a structure.
    #[test]
    fn a_constant_offset_is_scaled_by_the_width_it_transfers() {
        // STR r0,[r1,#4] - the field holds one, meaning one word.
        let (mut cpu, mut mem) = machine(&[0x6048]);
        cpu.regs.set(0, 0xAAAA);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA + 4), 0xAAAA, "one word on, not one byte");

        // STRB r0,[r1,#1] - here the field is bytes and is not scaled.
        let (mut cpu, mut mem) = machine(&[0x7048]);
        cpu.regs.set(0, 0x5A);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read8(DATA + 1), 0x5A);
    }

    #[test]
    fn a_halfword_has_its_own_form_scaled_by_two() {
        // STRH r0,[r1,#2] / LDRH r2,[r1,#2]
        let (mut cpu, mut mem) = machine(&[0x8048, 0x884A]);
        cpu.regs.set(0, 0xBEEF);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read16(DATA + 2), 0xBEEF);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xBEEF);
    }

    #[test]
    fn the_stack_pointer_has_transfers_of_its_own() {
        // STR r0,[sp,#4] / LDR r1,[sp,#4]
        let (mut cpu, mut mem) = machine(&[0x9001, 0x9901]);
        cpu.regs.set(0, 0xF00D);
        cpu.regs.set(13, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA + 4), 0xF00D);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(1), 0xF00D);
    }

    /// The address of something rather than the thing, from either the counter
    /// or the stack pointer.
    #[test]
    fn an_address_can_be_taken_without_reading_what_is_there() {
        // ADD r0, pc, #0
        let (mut cpu, mut mem) = machine(&[0xA000]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), BASE + 4, "rounded, as the pool load is");

        // ADD r0, sp, #8
        let (mut cpu, mut mem) = machine(&[0xA802]);
        cpu.regs.set(13, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), DATA + 8);
    }

    /// A function making room for its own variables. The sign is a bit, the
    /// constant being unsigned.
    #[test]
    fn the_stack_pointer_moves_by_a_constant_whose_sign_is_a_bit() {
        // SUB sp, #16
        let (mut cpu, mut mem) = machine(&[0xB084]);
        cpu.regs.set(13, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.sp(), DATA - 16);

        // ADD sp, #16
        let (mut cpu, mut mem) = machine(&[0xB004]);
        cpu.regs.set(13, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.sp(), DATA + 16);
    }

    /// `PUSH` and `POP` are the whole of a function's entry and exit, and the
    /// extra bit is what makes it so: pushing adds the link register and
    /// popping adds the *counter*, so a function returns by popping into it.
    #[test]
    fn a_function_enters_and_leaves_in_one_instruction_each() {
        // PUSH {r0-r2,lr}
        let (mut cpu, mut mem) = machine(&[0xB507]);
        for index in 0..3 {
            cpu.regs.set(index, 0x100 + index as u32);
        }
        cpu.regs.set(14, BASE + 0x40);
        cpu.regs.set(13, DATA + 0x40);
        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.sp(), DATA + 0x30, "four words down");
        assert_eq!(mem.read32(DATA + 0x30), 0x100, "the lowest register lowest");
        assert_eq!(mem.read32(DATA + 0x3C), BASE + 0x40, "and the link register last");

        // POP {r0-r2,pc} from the same place: the return. It goes into the same
        // memory the push wrote to, which is the whole point.
        mem.write16(BASE + 0x10, 0xBD07);
        let mut back = cpu.clone();
        back.regs.set_pc(BASE + 0x10);
        for index in 0..3 {
            back.regs.set(index, 0);
        }
        back.step(&mut mem).unwrap();

        assert_eq!(back.regs.get(0), 0x100, "the registers came back");
        assert_eq!(back.regs.pc(), BASE + 0x40, "and it returned");
        assert_eq!(back.regs.sp(), DATA + 0x40, "with the pointer back up");
    }

    #[test]
    fn a_block_transfer_walks_a_run_of_words() {
        // STMIA r4!, {r0-r2}
        let (mut cpu, mut mem) = machine(&[0xC407]);
        for index in 0..3 {
            cpu.regs.set(index, 0x200 + index as u32);
        }
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();

        assert_eq!(mem.read32(DATA), 0x200);
        assert_eq!(mem.read32(DATA + 8), 0x202);
        assert_eq!(cpu.regs.get(4), DATA + 12, "and the base walked");
    }

    /// The only conditional instruction left in the set.
    #[test]
    fn the_branch_is_the_one_thing_that_still_carries_a_condition() {
        // CMP r0,#0 / BEQ over the next one / MOV r1,#1 / MOV r2,#2
        //
        // An offset of nought already skips one instruction, the counter being
        // four bytes on: it is the *smallest* jump, not the absence of one.
        let (mut cpu, mut mem) = machine(&[0x2800, 0xD000, 0x2101, 0x2202]);
        cpu.regs.set(0, 0);
        for _ in 0..3 {
            cpu.step(&mut mem).unwrap();
        }
        assert_eq!(cpu.regs.get(1), 0, "the skipped instruction did not run");
        assert_eq!(cpu.regs.get(2), 2, "and the one after it did");
    }

    #[test]
    fn a_branch_backwards_is_how_a_loop_closes() {
        // MOV r0,#3 / SUB r0,#1 / BNE -4 / MOV r1,#7
        let program = [0x2003, 0x3801, 0xD1FD, 0x2107];
        let (mut cpu, mut mem) = machine(&program);

        let mut steps = 0;
        while cpu.regs.get(1) != 7 {
            cpu.step(&mut mem).unwrap();
            steps += 1;
            assert!(steps < 100, "the loop did not come out");
        }
        assert_eq!(cpu.regs.get(0), 0);
        assert_eq!(steps, 1 + 3 * 2 + 1, "three times round");
    }

    /// A call takes two instructions because sixteen bits cannot hold an offset
    /// that reaches the whole address space. Everything either half needs is in
    /// the link register, so an interrupt landing between them changes nothing.
    #[test]
    fn a_call_is_two_instructions_and_leaves_the_way_back_odd() {
        // BL +0x20, as the assembler splits it.
        let (mut cpu, mut mem) = machine(&[0xF000, 0xF80E]);
        cpu.step(&mut mem).unwrap();
        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.pc(), BASE + 0x20, "it reached the target");
        assert_eq!(cpu.regs.lr(), (BASE + 4) | 1, "with the way back, bottom bit set");
    }

    /// That bottom bit is what makes the return come back into THUMB.
    #[test]
    fn returning_through_the_link_register_comes_back_into_thumb() {
        let (mut cpu, mut mem) = machine(&[0xF000, 0xF80E]);
        cpu.step(&mut mem).unwrap();
        cpu.step(&mut mem).unwrap();

        // BX lr
        let (mut back, mut mem2) = machine(&[0x4770]);
        back.regs = cpu.regs.clone();
        back.regs.set_pc(BASE);
        back.step(&mut mem2).unwrap();

        assert!(back.regs.thumb(), "still THUMB");
        assert_eq!(back.regs.pc(), BASE + 4, "and back where the call was made");
    }

    /// The condition field of a branch has two values that are not conditions.
    #[test]
    fn the_two_encodings_that_are_not_conditions_are_not_treated_as_never() {
        // SWI 0
        let (mut cpu, mut mem) = machine(&[0xDF00]);
        cpu.regs.set_mode(Mode::User);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), 0x08, "it went to the vector");
        assert_eq!(cpu.regs.mode(), Mode::Supervisor);
        assert_eq!(cpu.regs.lr(), BASE + 2, "two on, not the ARM set's four");
        assert!(!cpu.regs.thumb(), "and into ARM to run the handler");

        // And the reserved one decodes to nothing rather than falling through.
        let (mut cpu, mut mem) = machine(&[0xDE00]);
        assert!(cpu.step(&mut mem).is_err());
    }

    /// Something end to end, in the shape compiled code actually has: a call to
    /// a function that adds two numbers on the stack and returns.
    #[test]
    fn a_call_that_uses_the_stack_and_returns_a_value() {
        let program = [
            0x2007, // MOV r0, #7
            0x2105, // MOV r1, #5
            0xF000, 0xF801, // BL +6, to the function below
            0xE7FE, // B . - where the caller settles
            // The function:
            0xB410, // PUSH {r4}
            0x1C04, // ADD r4, r0, #0
            0x1864, // ADD r4, r4, r1
            0x1C20, // ADD r0, r4, #0
            0xBC10, // POP {r4}
            0x4770, // BX lr
        ];
        let (mut cpu, mut mem) = machine(&program);
        cpu.regs.set(13, DATA + 0x40);
        cpu.regs.set(4, 0xDEAD);

        let mut steps = 0;
        while cpu.regs.pc() != BASE + 8 || steps == 0 {
            let before = cpu.regs.pc();
            cpu.step(&mut mem).unwrap();
            steps += 1;
            assert!(steps < 50, "it never came back");
            if cpu.regs.pc() == before {
                break;
            }
        }

        assert_eq!(cpu.regs.get(0), 12, "seven and five");
        assert_eq!(cpu.regs.get(4), 0xDEAD, "and the function put back what it borrowed");
        assert_eq!(cpu.regs.sp(), DATA + 0x40, "with the stack where it found it");
    }
}
