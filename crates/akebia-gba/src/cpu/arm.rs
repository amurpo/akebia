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
use super::Fault;

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
pub fn execute(regs: &mut Registers, addr: u32, instruction: u32) -> Result<(), Fault> {
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
        0b101 => {
            branch(regs, addr, instruction);
            Ok(())
        }
        0b000 | 0b001 => data_processing(regs, addr, instruction),
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
