//! Decoding and running an instruction in ARM state.
//!
//! # How an instruction is recognised
//!
//! Not by a table. The encoding is regular enough that the class falls out of a
//! handful of bits — mostly 27 to 25 — and irregular enough that three of the
//! classes are carved out of another one's space by patterns that would
//! otherwise decode as something legal. Those carve-outs are the whole
//! difficulty, and each is commented where it is tested for.
//!
//! The order below is therefore not arbitrary and cannot be sorted: the special
//! cases have to be asked about *before* the general case that would otherwise
//! swallow them.
//!
//! # The pipeline shows through
//!
//! Three stages are in flight, so by the time an instruction executes, the one
//! two places behind it has been fetched and `R15` already points there — eight
//! bytes on. Every read of `R15` as an operand therefore gets `+8`, and a
//! branch computes its target from that, not from itself.
//!
//! There is one exception, and it is the strangest corner of the instruction
//! set: when a data instruction takes its shift amount from a register, the
//! processor spends an extra cycle and the fetch runs one further ahead, so
//! `R15` reads as `+12` instead. It is not useful and no compiler emits it, but
//! test ROMs check it because it is the kind of thing an emulator gets wrong.

use super::alu::{self, Op};
use super::condition::Condition;
use super::registers::{Registers, T};
use super::shift::{self, Kind};
use super::{Bus, Fault};

/// How far ahead `R15` reads while an instruction executes.
const PIPELINE: u32 = 8;

/// And how far ahead it reads for the one instruction form that costs an extra
/// cycle before it looks.
const PIPELINE_SHIFTED: u32 = 12;

/// Runs one ARM instruction, which has already been fetched.
///
/// `addr` is where it came from, which is not `regs.pc()` — that has already
/// been moved on to the next one so that anything failing to branch simply
/// carries on.
pub fn execute(
    regs: &mut Registers,
    bus: &mut impl Bus,
    addr: u32,
    instruction: u32,
) -> Result<(), Fault> {
    if !Condition::from_bits(instruction >> 28).passes(regs) {
        // A failed condition costs the fetch and nothing else. In particular no
        // operand is read and no addressing mode writes anything back.
        return Ok(());
    }

    // Branch and exchange, which has to be asked about first: its encoding sits
    // inside the data-processing space and would otherwise decode as a `TEQ`
    // with nonsense operands.
    if instruction & 0x0FFF_FFF0 == 0x012F_FF10 {
        branch_and_exchange(regs, instruction);
        return Ok(());
    }

    match (instruction >> 25) & 0b111 {
        // The second carve-out. Bits 7 and 4 both set, in a space that would
        // otherwise be a data operation with a register-specified shift, means
        // one of the instructions that had nowhere else to go: the transfers
        // narrower than a word, the swap, and the multiplies.
        0b000 if instruction & 0b1001_0000 == 0b1001_0000 => {
            narrow_or_swap(regs, bus, addr, instruction)
        }
        0b000 | 0b001 => data_processing(regs, addr, instruction),
        // A register offset with bit 4 set is not a transfer at all: the
        // architecture reserves it, and a shift amount from a register is the
        // one thing an address cannot be built with.
        0b011 if instruction & 0b1_0000 != 0 => Err(Fault::Undefined { addr, instruction }),
        0b010 | 0b011 => {
            single_transfer(regs, bus, addr, instruction);
            Ok(())
        }
        0b101 => {
            branch(regs, addr, instruction);
            Ok(())
        }
        _ => Err(Fault::Undefined { addr, instruction }),
    }
}

/// `B` and `BL`.
///
/// The offset is a signed count of *instructions*, not bytes, which is what
/// buys the range: twenty-four bits reach thirty-two mebibytes rather than
/// sixteen.
fn branch(regs: &mut Registers, addr: u32, instruction: u32) {
    // Sign-extended from 24 bits, then scaled to bytes.
    let offset = ((instruction & 0x00FF_FFFF) << 8) as i32 >> 6;
    let target = addr.wrapping_add(PIPELINE).wrapping_add(offset as u32);

    if instruction & (1 << 24) != 0 {
        // The way back is the instruction after this one, which is *not* where
        // `R15` points: the link register gets `addr + 4` while `R15` reads
        // `addr + 8`.
        regs.set(14, addr.wrapping_add(4));
    }
    regs.set_pc(target & !3);
}

/// `BX`: branch, and change instruction set according to the bottom bit of the
/// address.
///
/// This is the only way into THUMB and the only way out, and the bottom bit is
/// the switch because no instruction on either side can start at an odd
/// address. A program calls it with the address of a function whose bottom bit
/// the linker set.
fn branch_and_exchange(regs: &mut Registers, instruction: u32) {
    let target = regs.get((instruction & 0xF) as usize);
    let thumb = target & 1 != 0;
    regs.set_thumb(thumb);
    // Whichever set it lands in, the address is aligned to it: two bytes for
    // THUMB and four for ARM. The bit that chose is not part of the address.
    regs.set_pc(if thumb { target & !1 } else { target & !3 });
}

/// The sixteen data operations, and the two instructions hiding among them.
fn data_processing(regs: &mut Registers, addr: u32, instruction: u32) -> Result<(), Fault> {
    let op = Op::from_bits(instruction >> 21);
    let sets_flags = instruction & (1 << 20) != 0;

    // A comparison that does not set flags computes nothing and stores nothing,
    // so the encoding would be wasted. It is used for the two instructions that
    // move the status registers about instead. This carve-out is why `TST`,
    // `TEQ`, `CMP` and `CMN` can never be written without their `S` bit.
    if !sets_flags && !op.writes_result() {
        return psr_transfer(regs, instruction);
    }

    let rn = ((instruction >> 16) & 0xF) as usize;
    let rd = ((instruction >> 12) & 0xF) as usize;
    let by_register_shift = instruction & 0x0200_0010 == 0x0000_0010;

    // The one place `R15` reads as twelve ahead rather than eight.
    let pc = addr.wrapping_add(if by_register_shift { PIPELINE_SHIFTED } else { PIPELINE });
    let read = |regs: &Registers, index: usize| {
        if index == 15 {
            pc
        } else {
            regs.get(index)
        }
    };

    let (operand, shifter_carry) = second_operand(regs, instruction, pc);
    let first = if op.reads_first() { read(regs, rn) } else { 0 };

    let out = alu::execute(op, first, operand, regs.c(), shifter_carry);

    if op.writes_result() {
        regs.set(rd, out.result);
    }

    if sets_flags {
        if rd == 15 && op.writes_result() {
            // Writing `R15` with the flags asked for is not a flag update at
            // all: it is how an exception returns. The saved status register
            // goes back whole, restoring the mode and the register bank along
            // with the flags, and the four condition bits this instruction
            // computed are discarded. `MOVS pc, lr` is the whole of a handler's
            // last line.
            regs.restore_cpsr();
        } else {
            regs.set_nz(out.result);
            regs.set_c(out.carry);
            if let Some(overflow) = out.overflow {
                regs.set_v(overflow);
            }
        }
    }

    Ok(())
}

/// The second operand: either a rotated constant or a register through the
/// shifter.
fn second_operand(regs: &Registers, instruction: u32, pc: u32) -> (u32, bool) {
    let read = |index: usize| if index == 15 { pc } else { regs.get(index) };

    if instruction & (1 << 25) != 0 {
        let out = shift::immediate((instruction >> 8) & 0xF, instruction & 0xFF, regs.c());
        return (out.value, out.carry);
    }

    let kind = Kind::from_bits(instruction >> 5);
    let value = read((instruction & 0xF) as usize);

    let out = if instruction & (1 << 4) != 0 {
        // By a register: only its bottom byte counts, and a zero in it means
        // no shift at all rather than one of the special encodings.
        shift::by_register(kind, read(((instruction >> 8) & 0xF) as usize), value, regs.c())
    } else {
        shift::by_immediate(kind, (instruction >> 7) & 0x1F, value, regs.c())
    };
    (out.value, out.carry)
}

/// Where a transfer reads or writes, and what its base register is left
/// holding.
///
/// The two are separate because the order they are applied in matters: a load
/// whose destination *is* its base has to end up holding what came out of
/// memory, not the address it came from.
struct Address {
    access: u32,
    /// The base register and its new value, when the addressing mode moves it.
    writeback: Option<(usize, u32)>,
}

/// Works out the address from the base, the offset and the four bits that say
/// how to put them together.
///
/// # The two indexing modes
///
/// *Pre*-indexed adds the offset and uses the result, optionally keeping it.
/// *Post*-indexed uses the base as it stands and then moves it, always keeping
/// it — which is what makes walking an array one instruction per element.
///
/// The write-back bit means something else entirely in the post-indexed form,
/// where the write-back is not optional: there it asks for the access to be
/// made with a user-mode program's rights from a privileged mode, which is how
/// an operating system touches memory on behalf of a caller without lending it
/// its own privileges. This machine has no memory protection, so nothing comes
/// of it here, and it is named rather than silently ignored.
fn addressing(instruction: u32, base: usize, base_value: u32, offset: u32) -> Address {
    let up = instruction & (1 << 23) != 0;
    let pre = instruction & (1 << 24) != 0;

    let moved = if up { base_value.wrapping_add(offset) } else { base_value.wrapping_sub(offset) };
    let keeps = if pre { instruction & (1 << 21) != 0 } else { true };

    Address {
        access: if pre { moved } else { base_value },
        writeback: if keeps { Some((base, moved)) } else { None },
    }
}

/// `LDR` and `STR`: a word or a byte.
fn single_transfer(regs: &mut Registers, bus: &mut impl Bus, addr: u32, instruction: u32) {
    let rn = ((instruction >> 16) & 0xF) as usize;
    let rd = ((instruction >> 12) & 0xF) as usize;
    let pc = addr.wrapping_add(PIPELINE);
    let read = |regs: &Registers, index: usize| if index == 15 { pc } else { regs.get(index) };

    // The bit that chooses between a constant and a register offset is the
    // opposite way round from the one in a data operation: here a set bit means
    // a register. Two instruction classes, two conventions, one bit position -
    // and an emulator that carries the data-processing reading over builds
    // every address out of the wrong thing.
    let offset = if instruction & (1 << 25) == 0 {
        instruction & 0xFFF
    } else {
        // A shifted register, but the shift amount is always a constant: there
        // is no room for a second register and no addressing mode wants one.
        let kind = Kind::from_bits(instruction >> 5);
        let value = read(regs, (instruction & 0xF) as usize);
        shift::by_immediate(kind, (instruction >> 7) & 0x1F, value, regs.c()).value
    };

    let at = addressing(instruction, rn, read(regs, rn), offset);
    let byte = instruction & (1 << 22) != 0;

    if instruction & (1 << 20) != 0 {
        // A load. The base moves first so that a load into its own base register
        // comes out holding what memory gave, which is what the hardware does
        // and the only reading that is any use.
        if let Some((index, value)) = at.writeback {
            regs.set(index, value);
        }
        let value = if byte {
            u32::from(bus.read8(at.access))
        } else {
            // The bus reads the aligned word; the rotation on top is the
            // processor's. An unaligned load does not fetch across the boundary
            // - it brings back the word the address is inside and turns it so
            // the addressed byte is at the bottom.
            bus.read32(at.access).rotate_right((at.access & 3) * 8)
        };
        regs.set(rd, value);
    } else {
        // A store of `R15` puts down twelve bytes on rather than the eight it
        // reads as everywhere else. It is a quirk of this processor and not of
        // the architecture, which leaves it open; nothing sensible relies on
        // it and test ROMs check it.
        let value = if rd == 15 { addr.wrapping_add(PIPELINE_SHIFTED) } else { regs.get(rd) };
        if byte {
            bus.write8(at.access, value as u8);
        } else {
            bus.write32(at.access, value);
        }
        if let Some((index, value)) = at.writeback {
            regs.set(index, value);
        }
    }
}

/// The transfers narrower than a word, and the swap, which share an encoding
/// carved out of the data-processing space.
fn narrow_or_swap(
    regs: &mut Registers,
    bus: &mut impl Bus,
    addr: u32,
    instruction: u32,
) -> Result<(), Fault> {
    match (instruction >> 5) & 0b11 {
        // The multiplies and the swap live here. Only the swap is written yet.
        0b00 => {
            if instruction & 0x0FB0_0FF0 == 0x0100_0090 {
                swap(regs, bus, instruction);
                Ok(())
            } else {
                Err(Fault::Undefined { addr, instruction })
            }
        }
        kind => {
            narrow_transfer(regs, bus, addr, instruction, kind);
            Ok(())
        }
    }
}

/// `LDRH`, `STRH`, `LDRSB` and `LDRSH`.
///
/// `kind` is the two bits that pick between them, already known not to be zero.
fn narrow_transfer(
    regs: &mut Registers,
    bus: &mut impl Bus,
    addr: u32,
    instruction: u32,
    kind: u32,
) {
    let rn = ((instruction >> 16) & 0xF) as usize;
    let rd = ((instruction >> 12) & 0xF) as usize;
    let pc = addr.wrapping_add(PIPELINE);
    let read = |regs: &Registers, index: usize| if index == 15 { pc } else { regs.get(index) };

    // A constant offset is split across the instruction in two nibbles, because
    // the bits in between were already spoken for by the register form.
    let offset = if instruction & (1 << 22) != 0 {
        ((instruction >> 4) & 0xF0) | (instruction & 0xF)
    } else {
        read(regs, (instruction & 0xF) as usize)
    };

    let at = addressing(instruction, rn, read(regs, rn), offset);

    if instruction & (1 << 20) == 0 {
        // The only store among them: a halfword. There is no signed store,
        // signedness being a question about how a value is widened and a store
        // widening nothing.
        let value = if rd == 15 { addr.wrapping_add(PIPELINE_SHIFTED) } else { regs.get(rd) };
        bus.write16(at.access, value as u16);
        if let Some((index, value)) = at.writeback {
            regs.set(index, value);
        }
        return;
    }

    if let Some((index, value)) = at.writeback {
        regs.set(index, value);
    }

    let value = match kind {
        // Unsigned halfword. From an odd address the hardware does something
        // that looks like nothing anybody wanted: it reads the halfword
        // underneath and rotates the whole word by eight, so the answer has the
        // addressed byte at the bottom and the other one up at the top.
        0b01 => u32::from(bus.read16(at.access)).rotate_right((at.access & 1) * 8),
        // Signed byte.
        0b10 => bus.read8(at.access) as i8 as u32,
        // Signed halfword - except from an odd address, where it is not a
        // halfword load at all. The processor gives up on the halfword and
        // sign-extends the single byte there instead, which is the strangest
        // documented behaviour in the instruction set and exactly what a test
        // ROM will ask about.
        _ if at.access & 1 != 0 => bus.read8(at.access) as i8 as u32,
        _ => bus.read16(at.access) as i16 as u32,
    };
    regs.set(rd, value);
}

/// `SWP`: read a word or a byte, put another in its place, and hand back what
/// was there.
///
/// On hardware the two accesses are one indivisible operation, which is the
/// whole point of it - it is how a lock is taken. Here nothing else can run in
/// between anyway, so the atomicity costs nothing to honour.
fn swap(regs: &mut Registers, bus: &mut impl Bus, instruction: u32) {
    let address = regs.get(((instruction >> 16) & 0xF) as usize);
    let rd = ((instruction >> 12) & 0xF) as usize;
    let source = regs.get((instruction & 0xF) as usize);

    if instruction & (1 << 22) != 0 {
        let was = bus.read8(address);
        bus.write8(address, source as u8);
        regs.set(rd, u32::from(was));
    } else {
        // The load rotates the same way any other unaligned word load does, and
        // the store goes to the aligned address underneath.
        let was = bus.read32(address).rotate_right((address & 3) * 8);
        bus.write32(address, source);
        regs.set(rd, was);
    }
}

/// `MRS` and `MSR`: reading and writing the status registers.
fn psr_transfer(regs: &mut Registers, instruction: u32) -> Result<(), Fault> {
    let saved = instruction & (1 << 22) != 0;

    if instruction & (1 << 21) == 0 {
        // `MRS`: the whole status register into a general one.
        let rd = ((instruction >> 12) & 0xF) as usize;
        regs.set(rd, if saved { regs.spsr() } else { regs.cpsr() });
        return Ok(());
    }

    // `MSR`. The source is a constant or a register, encoded the same way a
    // second operand is.
    let value = if instruction & (1 << 25) != 0 {
        shift::immediate((instruction >> 8) & 0xF, instruction & 0xFF, regs.c()).value
    } else {
        regs.get((instruction & 0xF) as usize)
    };

    // Four bits choose which bytes of the register are written, so a program
    // can set the flags without touching the mode or the other way about.
    // Without this, saving and restoring the flags around a critical section
    // would re-enable interrupts halfway through it.
    let mut mask = 0u32;
    for (bit, field) in [(16, 0x0000_00FF), (17, 0x0000_FF00), (18, 0x00FF_0000), (19, 0xFF00_0000)]
    {
        if instruction & (1 << bit) != 0 {
            mask |= field;
        }
    }

    if saved {
        let updated = (regs.spsr() & !mask) | (value & mask);
        regs.set_spsr(updated);
    } else {
        // In User mode the lower three bytes are refused however the mask
        // reads: an application cannot change the mode, unmask an interrupt or
        // put itself into THUMB by writing here.
        let mask = if regs.mode().is_privileged() { mask } else { mask & 0xFF00_0000 };
        let updated = (regs.cpsr() & !mask) | (value & mask);
        // The THUMB bit is not reachable this way even from a privileged mode.
        // Changing instruction set mid-instruction is what `BX` is for, and a
        // processor that allowed it here would carry on decoding the wrong
        // width until the next branch.
        let updated = (updated & !T) | (regs.cpsr() & T);
        regs.set_cpsr(updated);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Memory;
    use crate::cpu::{Bus, Cpu, Mode};

    /// Where the tests put their code. Internal RAM, because it is writable and
    /// the BIOS is not.
    const BASE: u32 = 0x0300_0000;

    /// A processor with a program in memory at [`BASE`], ready to run it.
    fn machine(program: &[u32]) -> (Cpu, Memory) {
        let mut mem = Memory::new();
        for (index, word) in program.iter().enumerate() {
            mem.write32(BASE + index as u32 * 4, *word);
        }
        let mut cpu = Cpu::new();
        cpu.regs.set_mode(Mode::System);
        cpu.regs.set_pc(BASE);
        (cpu, mem)
    }

    /// Runs exactly as many instructions as the program has words.
    fn run(program: &[u32]) -> (Cpu, Memory) {
        let (mut cpu, mut mem) = machine(program);
        for _ in 0..program.len() {
            cpu.step(&mut mem).expect("the program faulted");
        }
        (cpu, mem)
    }

    /// `MOV r0, #imm` — the instruction every other test is built out of.
    const fn mov_imm(rd: u32, imm: u32) -> u32 {
        0xE3A0_0000 | (rd << 12) | imm
    }

    #[test]
    fn a_constant_moves_into_a_register() {
        let (cpu, _) = run(&[mov_imm(0, 0x42), mov_imm(1, 0xFF)]);
        assert_eq!(cpu.regs.get(0), 0x42);
        assert_eq!(cpu.regs.get(1), 0xFF);
    }

    #[test]
    fn the_program_counter_walks_on_by_four() {
        let (cpu, _) = run(&[mov_imm(0, 1), mov_imm(0, 2), mov_imm(0, 3)]);
        assert_eq!(cpu.regs.pc(), BASE + 12);
    }

    /// An instruction whose condition fails costs the fetch and nothing else.
    #[test]
    fn a_failed_condition_leaves_everything_alone() {
        // MOVEQ r0, #0x42, with Z clear.
        let (mut cpu, mut mem) = machine(&[0x03A0_0042]);
        cpu.regs.set(0, 0xDEAD);
        cpu.regs.set_z(false);
        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.get(0), 0xDEAD, "the register was not written");
        assert_eq!(cpu.regs.pc(), BASE + 4, "and the counter still moved on");

        // And with Z set it happens.
        let (mut cpu, mut mem) = machine(&[0x03A0_0042]);
        cpu.regs.set_z(true);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x42);
    }

    /// A branch computes its target from where `R15` points, which is eight
    /// bytes on and not four. An emulator that uses the instruction's own
    /// address lands one instruction short of everything.
    #[test]
    fn a_branch_counts_from_eight_bytes_ahead() {
        // B +0: the offset is zero, so the target is the pipeline's own eight.
        let (mut cpu, mut mem) = machine(&[0xEA00_0000]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 8);

        // B -8, which is the branch to itself an idle loop is made of.
        let (mut cpu, mut mem) = machine(&[0xEAFF_FFFE]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE, "a branch to itself stays put");
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE, "and does not fall through on the second go");
    }

    /// The link register gets the instruction *after* the branch, which is four
    /// bytes on - not the eight that `R15` reads.
    #[test]
    fn a_linking_branch_keeps_the_way_back() {
        let (mut cpu, mut mem) = machine(&[0xEB00_0001]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.lr(), BASE + 4, "four on, not eight");
        assert_eq!(cpu.regs.pc(), BASE + 12, "and the target is eight on plus the offset");
    }

    /// `BX` reads the bottom bit of its target as the instruction set to land
    /// in, and that bit is not part of the address.
    #[test]
    fn branch_and_exchange_reads_the_bottom_bit_as_the_instruction_set() {
        // BX r0, with an even target: stays in ARM.
        let (mut cpu, mut mem) = machine(&[0xE12F_FF10]);
        cpu.regs.set(0, BASE + 0x40);
        cpu.step(&mut mem).unwrap();
        assert!(!cpu.regs.thumb());
        assert_eq!(cpu.regs.pc(), BASE + 0x40);

        // And with the bottom bit set: into THUMB, at the even address.
        let (mut cpu, mut mem) = machine(&[0xE12F_FF10]);
        cpu.regs.set(0, BASE + 0x41);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.thumb(), "the bottom bit chose THUMB");
        assert_eq!(cpu.regs.pc(), BASE + 0x40, "and is not part of the address");
    }

    /// `BX` sits inside the data-processing space and would decode as a `TEQ`
    /// if it were not asked about first.
    #[test]
    fn branch_and_exchange_is_not_mistaken_for_a_comparison() {
        let (mut cpu, mut mem) = machine(&[0xE12F_FF10]);
        cpu.regs.set(0, BASE + 0x40);
        cpu.regs.set_z(true);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 0x40, "it branched rather than comparing");
        assert!(cpu.regs.z(), "and left the flags alone");
    }

    /// Reading `R15` as an operand gives eight bytes on, which is how
    /// position-independent code finds its own data.
    #[test]
    fn reading_the_counter_as_an_operand_gives_eight_bytes_on() {
        // ADD r0, pc, #0
        let (mut cpu, mut mem) = machine(&[0xE28F_0000]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), BASE + 8);
    }

    /// Except when the shift amount comes from a register, where the extra
    /// cycle puts it four further on. No compiler emits this; test ROMs check
    /// it precisely because an emulator is likely to get it wrong.
    #[test]
    fn a_register_shift_pushes_the_counter_four_further() {
        // MOV r0, pc, LSL r1, with r1 holding zero: the value comes out
        // unshifted, so what lands in r0 is whatever `R15` read as.
        let (mut cpu, mut mem) = machine(&[0xE1A0_011F]);
        cpu.regs.set(1, 0);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), BASE + 12, "twelve, not eight");

        // And the same instruction with an immediate shift reads eight.
        let (mut cpu, mut mem) = machine(&[0xE1A0_000F]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), BASE + 8);
    }

    /// The arithmetic reaches the flags, and the flags reach the conditions.
    /// This is the loop every compiled `if` is made of.
    #[test]
    fn a_comparison_sets_the_flags_a_condition_then_reads() {
        // MOV r0,#5 / CMP r0,#5 / MOVEQ r1,#1 / MOVNE r1,#2
        let (cpu, _) = run(&[mov_imm(0, 5), 0xE350_0005, 0x03A0_1001, 0x13A0_1002]);
        assert!(cpu.regs.z(), "five equals five");
        assert!(cpu.regs.c(), "and the subtraction did not borrow");
        assert_eq!(cpu.regs.get(1), 1, "the equal arm ran and the other did not");
    }

    /// A comparison writes flags and no register, which is the whole of what
    /// distinguishes it from the operation it is built on.
    #[test]
    fn a_comparison_writes_no_register() {
        let (mut cpu, mut mem) = machine(&[0xE350_0005]);
        cpu.regs.set(0, 9);
        cpu.regs.set(12, 0xDEAD);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(12), 0xDEAD, "nothing was stored");
        assert!(!cpu.regs.z(), "but the flags moved");
    }

    /// Without its `S` bit an instruction computes and stores and says nothing
    /// about it.
    #[test]
    fn an_instruction_without_its_s_bit_leaves_the_flags_where_they_were() {
        // MOV r0, #0 without S: the result is zero and Z must not be set.
        let (mut cpu, mut mem) = machine(&[mov_imm(0, 0)]);
        cpu.regs.set_z(false);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0);
        assert!(!cpu.regs.z(), "a zero result did not set Z");

        // MOVS r0, #0 does.
        let (mut cpu, mut mem) = machine(&[0xE3B0_0000]);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.z());
    }

    /// `MRS` and `MSR` live in the space of comparisons written without their
    /// `S` bit, which would otherwise be four wasted encodings.
    #[test]
    fn the_status_register_can_be_read_out_and_put_back() {
        // MRS r0, cpsr
        let (mut cpu, mut mem) = machine(&[0xE10F_0000]);
        cpu.regs.set_n(true);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), cpu.regs.cpsr());
        assert!(cpu.regs.get(0) & 0x8000_0000 != 0, "the negative flag came out with it");
    }

    /// The field mask is what lets a program save and restore the flags without
    /// re-enabling an interrupt halfway through a critical section.
    #[test]
    fn a_status_write_touches_only_the_fields_it_names() {
        // MSR cpsr_f, r0 - the flags byte alone.
        let (mut cpu, mut mem) = machine(&[0xE128_F000]);
        cpu.regs.set_mode(Mode::Supervisor);
        cpu.regs.set(0, 0xF000_0000 | Mode::User.bits());
        cpu.step(&mut mem).unwrap();

        assert!(cpu.regs.n() && cpu.regs.z() && cpu.regs.c() && cpu.regs.v(), "the flags landed");
        assert_eq!(cpu.regs.mode(), Mode::Supervisor, "and the mode was not in the mask");
    }

    /// The mode *is* reachable when the mask names it, which is how a handler
    /// drops into System to reach the interrupted program's registers.
    #[test]
    fn a_status_write_that_names_the_control_field_changes_the_mode() {
        // MSR cpsr_c, r0
        let (mut cpu, mut mem) = machine(&[0xE121_F000]);
        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set(0, Mode::System.bits());
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.mode(), Mode::System);
    }

    /// An application cannot promote itself, unmask an interrupt or change
    /// instruction set by writing to the status register, however the mask
    /// reads.
    #[test]
    fn a_status_write_from_user_mode_reaches_the_flags_and_no_further() {
        // MSR cpsr_fc, r0 - asking for the flags and the control byte.
        let (mut cpu, mut mem) = machine(&[0xE129_F000]);
        cpu.regs.set_mode(Mode::User);
        // Unmasked to begin with, or the assertion below could not tell a write
        // that was refused from one that had nothing to do: a reset leaves both
        // interrupts masked.
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !crate::cpu::registers::I);
        cpu.regs.set(0, 0x8000_0000 | crate::cpu::registers::I | Mode::Supervisor.bits());
        cpu.step(&mut mem).unwrap();

        assert!(cpu.regs.n(), "the flags landed");
        assert_eq!(cpu.regs.mode(), Mode::User, "the mode did not");
        assert!(!cpu.regs.irq_disabled(), "nor the interrupt mask");
    }

    /// Changing instruction set mid-instruction is what `BX` is for. A
    /// processor that allowed it here would carry on decoding the wrong width
    /// until the next branch.
    #[test]
    fn the_instruction_set_cannot_be_changed_by_writing_the_status_register() {
        // MSR cpsr_c, r0 from a privileged mode, with the THUMB bit in it.
        let (mut cpu, mut mem) = machine(&[0xE121_F000]);
        cpu.regs.set(0, T | Mode::System.bits());
        cpu.step(&mut mem).unwrap();
        assert!(!cpu.regs.thumb(), "the THUMB bit was refused");
        assert_eq!(cpu.regs.mode(), Mode::System, "and the rest of the write went through");
    }

    /// Writing `R15` with the flags asked for is not a flag update: it is how
    /// an exception returns, and it restores the mode and the register bank
    /// along with them.
    #[test]
    fn moving_into_the_counter_with_flags_is_how_a_handler_returns() {
        // MOVS pc, lr
        let (mut cpu, mut mem) = machine(&[0xE1B0_F00E]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set(13, 0x0300_7F00);
        let interrupted = cpu.regs.cpsr();

        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set_spsr(interrupted);
        cpu.regs.set(13, 0x0300_7FA0);
        cpu.regs.set(14, BASE + 0x40);
        cpu.regs.set_pc(BASE);

        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 0x40, "it went where the link register pointed");
        assert_eq!(cpu.regs.mode(), Mode::User, "and came out of the handler's mode");
        assert_eq!(cpu.regs.sp(), 0x0300_7F00, "with the interrupted program's stack back");
    }

    /// The shifter is reached through the operand, so this is the one test that
    /// says the two are wired together at all.
    #[test]
    fn the_second_operand_goes_through_the_shifter() {
        // MOV r0,#3 / MOV r1, r0, LSL #4
        let (cpu, _) = run(&[mov_imm(0, 3), 0xE1A0_1200]);
        assert_eq!(cpu.regs.get(1), 0x30);
    }

    /// And a rotated constant is built the way the shifter builds it, which is
    /// how a 32-bit value fits in an instruction that has eight bits for it.
    #[test]
    fn a_constant_too_wide_for_the_field_arrives_rotated() {
        // MOV r0, #0xFF000000 - the byte 0xFF rotated right by eight.
        let (cpu, _) = run(&[0xE3A0_04FF]);
        assert_eq!(cpu.regs.get(0), 0xFF00_0000);
    }

    /// A bit pattern that decodes to nothing stops rather than being guessed
    /// at, and says where it was.
    #[test]
    fn something_that_decodes_to_nothing_says_so() {
        let (mut cpu, mut mem) = machine(&[0xF7FF_FFFF]);
        // The condition on that word is `NV`, which never runs: it is skipped
        // rather than faulted.
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4);

        // A coprocessor instruction, which this machine has none of.
        let (mut cpu, mut mem) = machine(&[0xEE00_0000]);
        let fault = cpu.step(&mut mem).unwrap_err();
        assert_eq!(fault, Fault::Undefined { addr: BASE, instruction: 0xEE00_0000 });
    }

    /// THUMB is not written yet, and says that rather than decoding a halfword
    /// as if it were a word.
    #[test]
    fn thumb_state_says_it_is_not_written_yet() {
        let (mut cpu, mut mem) = machine(&[mov_imm(0, 1)]);
        cpu.regs.set_thumb(true);
        assert_eq!(cpu.step(&mut mem).unwrap_err(), Fault::Thumb { addr: BASE });
    }

    /// Somewhere to load from and store to, well away from the code.
    const DATA: u32 = BASE + 0x400;

    #[test]
    fn a_word_goes_out_to_memory_and_comes_back() {
        // MOV r1,#<DATA offset> is awkward as a constant, so the base is set by
        // hand and the program is the two transfers alone.
        // STR r0,[r1] / LDR r2,[r1]
        let (mut cpu, mut mem) = machine(&[0xE581_0000, 0xE591_2000]);
        cpu.regs.set(0, 0x1234_5678);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), 0x1234_5678, "the store landed");
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x1234_5678, "and came back");
    }

    /// A byte transfer moves one byte and widens with zeros.
    #[test]
    fn a_byte_transfer_touches_one_byte_and_widens_with_zeros() {
        // STRB r0,[r1] / LDRB r2,[r1]
        let (mut cpu, mut mem) = machine(&[0xE5C1_0000, 0xE5D1_2000]);
        mem.write32(DATA, 0xFFFF_FFFF);
        cpu.regs.set(0, 0x1234_5678);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), 0xFFFF_FF78, "one byte and no other");
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x78, "and no sign came with it");
    }

    /// The bit that chooses a constant or a register offset is the opposite way
    /// round from the one in a data operation. Carrying that reading over builds
    /// every address out of the wrong thing.
    #[test]
    fn the_offset_bit_means_the_opposite_of_what_it_does_in_a_data_operation() {
        // LDR r2,[r1,#4] - bit 25 clear, so the twelve bits are the offset.
        let (mut cpu, mut mem) = machine(&[0xE591_2004]);
        mem.write32(DATA + 4, 0xCAFE);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xCAFE);

        // LDR r2,[r1,r3] - bit 25 set, so the offset is a register.
        let (mut cpu, mut mem) = machine(&[0xE791_2003]);
        mem.write32(DATA + 8, 0xBEEF);
        cpu.regs.set(1, DATA);
        cpu.regs.set(3, 8);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xBEEF);
    }

    /// A register offset goes through the shifter, which is how an array of
    /// words is indexed in one instruction.
    #[test]
    fn a_register_offset_passes_through_the_shifter() {
        // LDR r2,[r1,r3,LSL #2]
        let (mut cpu, mut mem) = machine(&[0xE791_2103]);
        mem.write32(DATA + 12, 0xF00D);
        cpu.regs.set(1, DATA);
        cpu.regs.set(3, 3);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xF00D, "three words on");
    }

    /// Down as well as up: the same offset, subtracted.
    #[test]
    fn an_offset_can_go_downwards() {
        // LDR r2,[r1,#-4]
        let (mut cpu, mut mem) = machine(&[0xE511_2004]);
        mem.write32(DATA - 4, 0xABCD);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xABCD);
    }

    /// Pre-indexed with write-back moves the base and uses the moved value.
    /// Post-indexed uses the base and then moves it. Between them they are how
    /// an array is walked one instruction per element.
    #[test]
    fn the_two_indexing_modes_differ_in_which_address_is_used() {
        mem_pair(0xE5B1_2004, DATA + 4, "pre-indexed uses the moved address");
        mem_pair(0xE491_2004, DATA, "post-indexed uses the base as it stands");
    }

    /// Both of the above leave the base moved by four.
    fn mem_pair(instruction: u32, expected_at: u32, why: &str) {
        let (mut cpu, mut mem) = machine(&[instruction]);
        mem.write32(expected_at, 0x5A5A);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x5A5A, "{why}");
        assert_eq!(cpu.regs.get(1), DATA + 4, "{why}: and the base moved");
    }

    /// Pre-indexed *without* write-back leaves the base alone, which is the
    /// ordinary way a structure field is reached.
    #[test]
    fn an_offset_without_write_back_leaves_the_base_where_it_was() {
        // LDR r2,[r1,#4]
        let (mut cpu, mut mem) = machine(&[0xE591_2004]);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(1), DATA, "untouched");
    }

    /// A load into its own base register has to come out holding what memory
    /// gave, not the address it came from. The order the two writes happen in
    /// is the whole of it.
    #[test]
    fn a_load_into_its_own_base_keeps_what_memory_gave() {
        // LDR r1,[r1],#4 - post-indexed, so the base would move to DATA+4.
        let (mut cpu, mut mem) = machine(&[0xE491_1004]);
        mem.write32(DATA, 0xD0D0_D0D0);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(1), 0xD0D0_D0D0, "the load won, not the write-back");
    }

    /// An unaligned load does not fetch across the boundary. It brings back the
    /// word its address is inside and turns it so the addressed byte is at the
    /// bottom - which is the processor's doing, not the memory's.
    #[test]
    fn an_unaligned_word_load_comes_back_rotated() {
        for (skew, wanted) in
            [(0u32, 0x1122_3344u32), (1, 0x4411_2233), (2, 0x3344_1122), (3, 0x2233_4411)]
        {
            // LDR r2,[r1]
            let (mut cpu, mut mem) = machine(&[0xE591_2000]);
            mem.write32(DATA, 0x1122_3344);
            cpu.regs.set(1, DATA + skew);
            cpu.step(&mut mem).unwrap();
            assert_eq!(cpu.regs.get(2), wanted, "skewed by {skew}");
        }
    }

    /// A halfword, and the odd-address behaviour that looks like nothing
    /// anybody wanted: the whole word rotates by eight.
    #[test]
    fn a_halfword_loads_and_stores_and_rotates_when_the_address_is_odd() {
        // STRH r0,[r1] / LDRH r2,[r1]
        let (mut cpu, mut mem) = machine(&[0xE1C1_00B0, 0xE1D1_20B0]);
        mem.write32(DATA, 0xFFFF_FFFF);
        cpu.regs.set(0, 0x1234_5678);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), 0xFFFF_5678, "two bytes and no more");
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x5678, "and came back widened with zeros");

        // From an odd address: the halfword underneath, rotated by eight.
        let (mut cpu, mut mem) = machine(&[0xE1D1_20B0]);
        mem.write32(DATA, 0x0000_ABCD);
        cpu.regs.set(1, DATA + 1);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xCD00_00AB, "the whole word turned by eight");
    }

    /// A signed byte is widened by its top bit, which is what makes it signed.
    #[test]
    fn a_signed_byte_arrives_widened_by_its_own_sign() {
        // LDRSB r2,[r1]
        let (mut cpu, mut mem) = machine(&[0xE1D1_20D0]);
        mem.write32(DATA, 0xFF);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xFFFF_FFFF, "-1 and not 255");

        let (mut cpu, mut mem) = machine(&[0xE1D1_20D0]);
        mem.write32(DATA, 0x7F);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x7F, "and a positive one is left alone");
    }

    /// A signed halfword from an *odd* address is not a halfword load at all:
    /// the processor gives up and sign-extends the single byte there. It is the
    /// strangest documented behaviour in the instruction set.
    #[test]
    fn a_signed_halfword_from_an_odd_address_is_a_signed_byte_instead() {
        // LDRSH r2,[r1], aligned: a halfword, widened by its sign.
        let (mut cpu, mut mem) = machine(&[0xE1D1_20F0]);
        mem.write32(DATA, 0x0000_8123);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xFFFF_8123, "the halfword, sign-extended");

        // The same instruction one byte on: the byte at DATA+1, which is 0x81.
        let (mut cpu, mut mem) = machine(&[0xE1D1_20F0]);
        mem.write32(DATA, 0x0000_8123);
        cpu.regs.set(1, DATA + 1);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xFFFF_FF81, "one byte, sign-extended, not a halfword");
    }

    /// A halfword transfer sits in the space a data operation with a
    /// register-specified shift would occupy, and has to be caught before it.
    #[test]
    fn a_halfword_transfer_is_not_mistaken_for_a_data_operation() {
        let (mut cpu, mut mem) = machine(&[0xE1D1_20B0]);
        mem.write32(DATA, 0xBEEF);
        cpu.regs.set(1, DATA);
        cpu.regs.set(2, 0xDEAD);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0xBEEF, "it loaded rather than computing");
    }

    /// Storing `R15` puts down twelve bytes on, not the eight it reads as
    /// everywhere else. A quirk of this processor, which the architecture
    /// leaves open.
    #[test]
    fn storing_the_counter_puts_down_twelve_bytes_on() {
        // STR pc,[r1]
        let (mut cpu, mut mem) = machine(&[0xE581_F000]);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), BASE + 12);
    }

    /// And using it as a *base* reads the ordinary eight, which is how a
    /// constant too wide for an instruction is fetched from just after the code.
    #[test]
    fn loading_through_the_counter_reads_the_ordinary_eight() {
        // LDR r0,[pc,#0] - the word two instructions on.
        let (mut cpu, mut mem) = machine(&[0xE59F_0000, 0, 0x1234_5678]);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x1234_5678);
    }

    /// A failed condition writes nothing back, which matters because the
    /// addressing mode has a side effect of its own.
    #[test]
    fn a_transfer_that_does_not_happen_does_not_move_its_base_either() {
        // LDREQ r2,[r1],#4 with Z clear.
        let (mut cpu, mut mem) = machine(&[0x0491_2004]);
        cpu.regs.set(1, DATA);
        cpu.regs.set_z(false);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(1), DATA, "the base did not move");
    }

    /// `SWP` reads and writes in one go, which is how a lock is taken.
    #[test]
    fn a_swap_hands_back_what_was_there_and_leaves_the_new_value() {
        // SWP r2,r0,[r1]
        let (mut cpu, mut mem) = machine(&[0xE101_2090]);
        mem.write32(DATA, 0x1111_1111);
        cpu.regs.set(0, 0x2222_2222);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x1111_1111, "what was there came back");
        assert_eq!(mem.read32(DATA), 0x2222_2222, "and the new value went in");
    }

    /// Walking an array, which is what post-indexing exists for: three loads
    /// and one register that moves itself.
    #[test]
    fn an_array_is_walked_one_instruction_per_element() {
        // LDR r2,[r1],#4 three times over, adding into r3.
        let program = [0xE491_2004, 0xE083_3002, 0xE491_2004, 0xE083_3002, 0xE491_2004, 0xE083_3002];
        let (mut cpu, mut mem) = machine(&program);
        for (index, value) in [10u32, 20, 30].iter().enumerate() {
            mem.write32(DATA + index as u32 * 4, *value);
        }
        cpu.regs.set(1, DATA);
        for _ in 0..program.len() {
            cpu.step(&mut mem).unwrap();
        }
        assert_eq!(cpu.regs.get(3), 60, "ten and twenty and thirty");
        assert_eq!(cpu.regs.get(1), DATA + 12, "and the base walked all three");
    }

    /// Something end to end: counting down to zero, which needs the ALU, the
    /// flags, a condition and a backwards branch all agreeing.
    #[test]
    fn a_loop_counts_down_and_comes_out() {
        // MOV r0,#10 / SUBS r0,r0,#1 / BNE back to the SUBS / MOV r1,#7
        //
        // The branch's offset is -12 and not -8: it counts from eight bytes on,
        // so coming back one instruction means going back three.
        let program = [mov_imm(0, 10), 0xE250_0001, 0x1AFF_FFFD, mov_imm(1, 7)];
        let (mut cpu, mut mem) = machine(&program);

        let mut steps = 0;
        while cpu.regs.get(1) != 7 {
            cpu.step(&mut mem).expect("the program faulted");
            steps += 1;
            assert!(steps < 100, "the loop did not come out");
        }
        assert_eq!(cpu.regs.get(0), 0, "it counted all the way down");
        assert_eq!(steps, 1 + 10 * 2 + 1, "ten times round, and once through each end");
    }
}
