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
        0b100 => {
            block_transfer(regs, bus, addr, instruction);
            Ok(())
        }
        0b101 => {
            branch(regs, addr, instruction);
            Ok(())
        }
        0b111 if instruction & 0x0F00_0000 == 0x0F00_0000 => {
            software_interrupt(regs, addr.wrapping_add(4));
            Ok(())
        }
        // What is left is the coprocessor space, and this machine has none.
        _ => Err(Fault::Undefined { addr, instruction }),
    }
}

/// `LDM` and `STM`: up to sixteen registers in one instruction.
///
/// # The rule that makes the four modes one piece of code
///
/// **Registers always go in ascending order at ascending addresses.** The
/// lowest-numbered register is always at the lowest address, whichever
/// direction the instruction is written in. The direction bits do not reverse
/// the order — they only decide *where the block starts*.
///
/// That is what lets a stack be pushed with one mode and popped with its
/// opposite and come back in the right order, and it is why the code below
/// works out the bottom of the block first and then walks upward, rather than
/// walking whichever way the instruction seems to say.
fn block_transfer(regs: &mut Registers, bus: &mut impl Bus, addr: u32, instruction: u32) {
    let rn = ((instruction >> 16) & 0xF) as usize;
    let load = instruction & (1 << 20) != 0;
    let up = instruction & (1 << 23) != 0;
    let pre = instruction & (1 << 24) != 0;
    let base = regs.get(rn);

    // An empty list is not a no-op. This processor transfers `R15` alone and
    // moves the base by the full sixteen registers' worth, which is what a
    // compiler never emits and a test ROM always asks about.
    let listed = instruction & 0xFFFF;
    let (list, count) = if listed == 0 { (0x8000, 16) } else { (listed, listed.count_ones()) };
    let span = count * 4;

    // The bottom of the block, from which everything is counted upward.
    let bottom = if up {
        if pre { base.wrapping_add(4) } else { base }
    } else if pre {
        base.wrapping_sub(span)
    } else {
        base.wrapping_sub(span).wrapping_add(4)
    };
    let moved = if up { base.wrapping_add(span) } else { base.wrapping_sub(span) };

    // The `S` bit asks for the registers a *user-mode* program would see, so
    // that a handler can save the registers of what it interrupted rather than
    // its own. The exception is a load that includes `R15`, where `S` means
    // something else entirely — see below — and the banked registers are the
    // ones wanted.
    let restores_status = load && instruction & (1 << 22) != 0 && list & 0x8000 != 0;
    let user_bank = instruction & (1 << 22) != 0 && !restores_status;
    let writes_back = instruction & (1 << 21) != 0;
    let mode = regs.mode();
    if user_bank {
        regs.set_mode(super::Mode::User);
    }

    let mut at = bottom;
    let mut first = true;
    for index in 0..16usize {
        if list & (1 << index) == 0 {
            continue;
        }
        if load {
            let value = bus.read32(at);
            regs.set(index, value);
        } else {
            let value = if index == 15 {
                // The same twelve as a single store.
                addr.wrapping_add(PIPELINE_SHIFTED)
            } else if index == rn && writes_back && !first {
                // Storing the base register itself puts down the *moved* value,
                // unless it is the first one out — in which case the write-back
                // has not happened yet and the old value goes down. With no
                // write-back asked for there is no moved value to put down at
                // all. Nothing sensible depends on any of this and it is
                // exactly the sort of thing a test ROM exists to catch.
                moved
            } else {
                regs.get(index)
            };
            bus.write32(at, value);
        }
        at = at.wrapping_add(4);
        first = false;
    }

    if user_bank {
        regs.set_mode(mode);
    }

    // The base moves after the transfer, and a load that brought a new value
    // into the base register keeps that value: the write-back loses.
    if writes_back && !(load && list & (1 << rn) != 0) {
        regs.set(rn, moved);
    }

    if restores_status {
        // `LDM` with `R15` in the list and `S` set is a return from an
        // exception, exactly as `MOVS pc, lr` is: the saved status register
        // goes back whole, mode and register bank with it. It is how a handler
        // that pushed its working registers gets out in one instruction.
        regs.restore_cpsr();
    }

    // A word loaded into `R15` is a branch, and the address is aligned to
    // whichever instruction set it lands in — which the restore above may just
    // have changed.
    if load && list & 0x8000 != 0 {
        let target = regs.pc();
        regs.set_pc(if regs.thumb() { target & !1 } else { target & !3 });
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
        if rd == 15 {
            // Naming `R15` with the flags asked for is not a flag update at
            // all: it is how an exception returns. The saved status register
            // goes back whole, restoring the mode and the register bank along
            // with the flags, and the four condition bits this instruction
            // computed are discarded. `MOVS pc, lr` is the whole of a handler's
            // last line.
            //
            // It is the *destination field* that decides this, not whether the
            // instruction has anything to put there. A comparison has no
            // destination — its four bits are spare — and one written with
            // fifteen in them still restores the status register, without
            // writing a result and without disturbing the program counter. No
            // assembler emits that and no compiler would, which is exactly why
            // a test suite asks: it is where an emulator that reasoned about
            // intent rather than about bits gives a different machine.
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
        // Three instructions share this corner, told apart by bits that mean
        // nothing anywhere else. The order does not matter here because the
        // three patterns are disjoint, unlike the carve-outs above.
        0b00 => {
            if instruction & 0x0FC0_00F0 == 0x0000_0090 {
                multiply(regs, instruction);
                Ok(())
            } else if instruction & 0x0F80_00F0 == 0x0080_0090 {
                multiply_long(regs, instruction);
                Ok(())
            } else if instruction & 0x0FB0_0FF0 == 0x0100_0090 {
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

/// `MUL` and `MLA`: a 32-bit product, and optionally something added to it.
///
/// # Why the register fields are in the wrong places
///
/// Because this instruction was added to an encoding that was already full. The
/// destination sits where every other instruction keeps its *first operand*,
/// and the operands are scattered through the fields left over. Reading them by
/// the usual positions gets four registers, all of them wrong, and the answer
/// still lands somewhere plausible — which is why this is worth saying out loud
/// rather than trusting the shifts below to speak for themselves.
fn multiply(regs: &mut Registers, instruction: u32) {
    let rd = ((instruction >> 16) & 0xF) as usize;
    let rn = ((instruction >> 12) & 0xF) as usize;
    let rs = ((instruction >> 8) & 0xF) as usize;
    let rm = (instruction & 0xF) as usize;

    // Wrapping, and signedness does not come into it: the low thirty-two bits
    // of a product are the same whichever way the operands are read.
    let mut result = regs.get(rm).wrapping_mul(regs.get(rs));
    if instruction & (1 << 21) != 0 {
        result = result.wrapping_add(regs.get(rn));
    }
    regs.set(rd, result);

    if instruction & (1 << 20) != 0 {
        // Only two of the four flags. The architecture says the carry is left
        // holding a meaningless value, and the honest reading of "meaningless"
        // is to leave it alone rather than invent a rule for it: a game cannot
        // depend on what is not defined, and a wrong rule would be a wrong
        // answer where none is owed.
        regs.set_nz(result);
    }
}

/// `UMULL`, `UMLAL`, `SMULL` and `SMLAL`: a 64-bit product across two
/// registers.
fn multiply_long(regs: &mut Registers, instruction: u32) {
    let high = ((instruction >> 16) & 0xF) as usize;
    let low = ((instruction >> 12) & 0xF) as usize;
    let rs = ((instruction >> 8) & 0xF) as usize;
    let rm = (instruction & 0xF) as usize;

    // Here the signedness does matter, because the top half of the product is
    // what the sign reaches.
    let product = if instruction & (1 << 22) != 0 {
        ((regs.get(rm) as i32 as i64).wrapping_mul(regs.get(rs) as i32 as i64)) as u64
    } else {
        u64::from(regs.get(rm)).wrapping_mul(u64::from(regs.get(rs)))
    };

    let result = if instruction & (1 << 21) != 0 {
        let existing = (u64::from(regs.get(high)) << 32) | u64::from(regs.get(low));
        product.wrapping_add(existing)
    } else {
        product
    };

    regs.set(low, result as u32);
    regs.set(high, (result >> 32) as u32);

    if instruction & (1 << 20) != 0 {
        // The sign is the top bit of the whole 64-bit answer and the zero is
        // both halves being zero, so neither flag can be read off one register.
        regs.set_n(result & 0x8000_0000_0000_0000 != 0);
        regs.set_z(result == 0);
    }
}

/// `SWI`: the instruction a program calls the BIOS with.
///
/// It is an exception raised on purpose, and it goes through the same door as
/// any other: the status register is saved, the mode changes to the one with
/// the rights, interrupts are masked so the handler's first instructions cannot
/// be interrupted before it has a stack, and the processor jumps to a fixed
/// address near the bottom of memory.
///
/// The twenty-four bits below the opcode are not read by the processor at all.
/// They are a message to the handler, which fetches the instruction back out of
/// memory to see which service was asked for — which is why the link register
/// has to point just past it.
///
/// `return_to` is where the caller resumes, which is not the same distance on
/// in the two instruction sets — four bytes in ARM and two in THUMB — so it is
/// worked out by the caller rather than assumed here.
pub(super) fn software_interrupt(regs: &mut Registers, return_to: u32) {
    let caller = regs.cpsr();
    regs.set_mode(super::Mode::Supervisor);
    regs.set_spsr(caller);
    regs.set(14, return_to);

    // Into ARM state with interrupts masked, whatever the caller was doing.
    let status = regs.cpsr();
    regs.set_cpsr((status | super::registers::I) & !T);
    regs.set_pc(SWI_VECTOR);
}

/// Where the processor goes on `SWI`, fixed in the hardware.
const SWI_VECTOR: u32 = 0x08;

/// And where it goes when something interrupts it.
const IRQ_VECTOR: u32 = 0x18;

/// Interrupts the processor, which is the same door an `SWI` goes through with
/// a different mode and a different vector.
///
/// # Why the way back is four bytes further on than it needs to be
///
/// Because a handler returns with `SUBS pc, lr, #4`, and that four is not an
/// adjustment somebody chose — it is what makes the return work out for an
/// exception raised by a *failed* access, where the instruction has to be tried
/// again rather than stepped over. The same return instruction serves both, so
/// an interrupt has to leave the link register in the shape that instruction
/// expects.
pub(super) fn enter_irq(regs: &mut Registers) {
    let caller = regs.cpsr();
    let resume_at = regs.pc();
    regs.set_mode(super::Mode::Irq);
    regs.set_spsr(caller);
    regs.set(14, resume_at.wrapping_add(4));

    let status = regs.cpsr();
    regs.set_cpsr((status | super::registers::I) & !T);
    regs.set_pc(IRQ_VECTOR);
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

    /// The same rule, asked of an instruction that has no result to write. A
    /// comparison's destination field is spare, so nothing reads it — except
    /// this. The processor restores the status register on the four bits alone,
    /// leaving the counter walking on as any comparison would. No assembler
    /// emits it, which is why the ROM that checks it spells the word out.
    #[test]
    fn a_comparison_that_names_the_counter_restores_the_status_register_anyway() {
        // dw 0xE15FF000 - "CMPS pc, r0", with fifteen in the destination field.
        let (mut cpu, mut mem) = machine(&[0xE15F_F000]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set_n(true);
        cpu.regs.set_z(true);
        cpu.regs.set(13, 0x0300_7F00);
        let interrupted = cpu.regs.cpsr();

        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set_spsr(interrupted);
        cpu.regs.set(13, 0x0300_7FA0);
        cpu.regs.set_pc(BASE);
        // The comparison itself would clear both flags: the counter read as
        // eight ahead is neither zero nor negative.
        cpu.regs.set(0, 0);

        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4, "it wrote no result, so the counter walked on");
        assert_eq!(cpu.regs.mode(), Mode::User, "and yet the mode came back");
        assert_eq!(cpu.regs.sp(), 0x0300_7F00, "with the bank that goes with it");
        assert!(cpu.regs.n() && cpu.regs.z(), "the flags are the saved ones, not the computed ones");
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

    /// Registers always go in ascending order at ascending addresses, whichever
    /// direction the instruction is written in. It is what lets a stack be
    /// pushed with one mode and popped with its opposite.
    #[test]
    fn registers_go_in_ascending_order_whichever_way_the_block_runs() {
        // STMIA r4!, {r0-r2}  then read the three words back by hand.
        let (mut cpu, mut mem) = machine(&[0xE8A4_0007]);
        cpu.regs.set(0, 0xAAAA);
        cpu.regs.set(1, 0xBBBB);
        cpu.regs.set(2, 0xCCCC);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();

        assert_eq!(mem.read32(DATA), 0xAAAA, "the lowest register at the lowest address");
        assert_eq!(mem.read32(DATA + 4), 0xBBBB);
        assert_eq!(mem.read32(DATA + 8), 0xCCCC);
        assert_eq!(cpu.regs.get(4), DATA + 12, "and the base moved past all three");

        // STMDB r4!, {r0-r2}: the block ends where the base was, and the order
        // inside it is the same.
        let (mut cpu, mut mem) = machine(&[0xE924_0007]);
        cpu.regs.set(0, 0xAAAA);
        cpu.regs.set(1, 0xBBBB);
        cpu.regs.set(2, 0xCCCC);
        cpu.regs.set(4, DATA + 12);
        cpu.step(&mut mem).unwrap();

        assert_eq!(mem.read32(DATA), 0xAAAA, "still the lowest register lowest");
        assert_eq!(mem.read32(DATA + 8), 0xCCCC);
        assert_eq!(cpu.regs.get(4), DATA, "and the base came down by three words");
    }

    /// The four modes differ only in where the block starts.
    #[test]
    fn the_four_modes_put_the_block_in_four_places() {
        // Each stores r0 alone, from a base of DATA + 4, and says where it went.
        for (name, instruction, at, after) in [
            ("increment after", 0xE8A4_0001u32, DATA + 4, DATA + 8),
            ("increment before", 0xE9A4_0001, DATA + 8, DATA + 8),
            ("decrement after", 0xE824_0001, DATA + 4, DATA),
            ("decrement before", 0xE924_0001, DATA, DATA),
        ] {
            let (mut cpu, mut mem) = machine(&[instruction]);
            cpu.regs.set(0, 0x1234);
            cpu.regs.set(4, DATA + 4);
            cpu.step(&mut mem).unwrap();
            assert_eq!(mem.read32(at), 0x1234, "{name}: the word went to the wrong place");
            assert_eq!(cpu.regs.get(4), after, "{name}: the base ended up wrong");
        }
    }

    /// A push and a pop written as opposites bring everything back where it
    /// was, which is the whole reason the ordering rule exists.
    #[test]
    fn a_push_and_its_opposite_pop_come_back_to_where_they_started() {
        // STMDB sp!, {r0-r3}  /  LDMIA sp!, {r0-r3}
        let (mut cpu, mut mem) = machine(&[0xE92D_000F, 0xE8BD_000F]);
        for index in 0..4 {
            cpu.regs.set(index, 0x1000 + index as u32);
        }
        cpu.regs.set(13, DATA + 0x40);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.sp(), DATA + 0x30, "four words down");

        for index in 0..4 {
            cpu.regs.set(index, 0);
        }
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.sp(), DATA + 0x40, "and back up");
        for index in 0..4 {
            assert_eq!(cpu.regs.get(index), 0x1000 + index as u32, "r{index} came back");
        }
    }

    /// A load that brings a new value into its own base register keeps that
    /// value: the write-back loses, as it does for a single transfer.
    #[test]
    fn a_block_load_into_its_own_base_keeps_what_memory_gave() {
        // LDMIA r4!, {r4}
        let (mut cpu, mut mem) = machine(&[0xE8B4_0010]);
        mem.write32(DATA, 0xFEED_FACE);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(4), 0xFEED_FACE, "not DATA + 4");
    }

    /// Storing the base register itself puts down the moved value, unless it is
    /// the first one out - where the write-back has not happened yet.
    #[test]
    fn storing_the_base_puts_down_the_old_value_only_if_it_goes_first() {
        // STMIA r0!, {r0,r1} - r0 is the lowest, so it goes first, so the old
        // value goes down.
        let (mut cpu, mut mem) = machine(&[0xE8A0_0003]);
        cpu.regs.set(0, DATA);
        cpu.regs.set(1, 0x1111);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), DATA, "first out: the old base");

        // STMIA r1!, {r0,r1} - now r1 is second, so the moved value goes down.
        let (mut cpu, mut mem) = machine(&[0xE8A1_0003]);
        cpu.regs.set(0, 0x2222);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA + 4), DATA + 8, "not first: the moved base");
    }

    /// An empty list is not a no-op. This processor transfers `R15` alone and
    /// moves the base by the full sixteen registers' worth.
    #[test]
    fn an_empty_list_transfers_the_counter_and_moves_the_base_by_sixteen() {
        // STMIA r4!, {} - the encoding with no registers named.
        let (mut cpu, mut mem) = machine(&[0xE8A4_0000]);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), BASE + 12, "the counter went down, twelve bytes on");
        assert_eq!(cpu.regs.get(4), DATA + 0x40, "and the base moved by sixteen words");

        // LDMIA r4!, {} - and it comes back as a branch.
        let (mut cpu, mut mem) = machine(&[0xE8B4_0000]);
        mem.write32(DATA, BASE + 0x80);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 0x80, "it branched");
        assert_eq!(cpu.regs.get(4), DATA + 0x40);
    }

    /// A word loaded into the counter is a branch.
    #[test]
    fn a_block_load_into_the_counter_branches() {
        // LDMIA r4, {r15}
        let (mut cpu, mut mem) = machine(&[0xE894_8000]);
        mem.write32(DATA, BASE + 0x40);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 0x40);
    }

    /// Storing the counter puts down twelve bytes on, the same as a single
    /// store does.
    #[test]
    fn a_block_store_of_the_counter_puts_down_twelve_bytes_on() {
        // STMIA r4, {r15}
        let (mut cpu, mut mem) = machine(&[0xE884_8000]);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), BASE + 12);
    }

    /// The `S` bit asks for the registers a user-mode program would see, so a
    /// handler can save what it interrupted rather than its own.
    #[test]
    fn the_status_bit_reaches_past_the_banking_to_the_users_registers() {
        // STMIA r4, {r13,r14}^ - the caret is the S bit.
        let (mut cpu, mut mem) = machine(&[0xE8C4_6000]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set(13, 0x1111_1111);
        cpu.regs.set(14, 0x2222_2222);

        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set(13, 0x3333_3333);
        cpu.regs.set(14, 0x4444_4444);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();

        assert_eq!(mem.read32(DATA), 0x1111_1111, "the user's stack pointer, not the handler's");
        assert_eq!(mem.read32(DATA + 4), 0x2222_2222);
        assert_eq!(cpu.regs.sp(), 0x3333_3333, "and the handler's own is untouched");
        assert_eq!(cpu.regs.mode(), Mode::Irq, "as is the mode it was in");
    }

    /// The base register is the current mode's even when the list is the
    /// user's, because the address is the handler's business.
    #[test]
    fn the_base_of_a_user_bank_transfer_is_still_the_current_modes() {
        // STMIA r13, {r0}^ - r13 as base, in a mode where it is banked.
        let (mut cpu, mut mem) = machine(&[0xE8CD_0001]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set(13, 0xDEAD_BEEF);
        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set(13, DATA);
        cpu.regs.set(0, 0x9999);
        cpu.step(&mut mem).unwrap();
        assert_eq!(mem.read32(DATA), 0x9999, "the handler's base was used");
    }

    /// `LDM` with the counter in the list and `S` set is a return from an
    /// exception: the saved status register goes back whole, mode and bank with
    /// it. A handler gets out in one instruction.
    #[test]
    fn a_block_load_with_the_counter_and_the_status_bit_returns_from_a_handler() {
        // LDMIA r4!, {r0,r15}^
        let (mut cpu, mut mem) = machine(&[0xE8F4_8001]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set(13, 0x0300_7F00);
        let interrupted = cpu.regs.cpsr();

        cpu.regs.set_mode(Mode::Irq);
        cpu.regs.set_spsr(interrupted);
        cpu.regs.set(13, 0x0300_7FA0);
        mem.write32(DATA, 0x1234);
        mem.write32(DATA + 4, BASE + 0x40);
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.get(0), 0x1234, "the working register came back");
        assert_eq!(cpu.regs.pc(), BASE + 0x40, "and it branched");
        assert_eq!(cpu.regs.mode(), Mode::User, "out of the handler's mode");
        assert_eq!(cpu.regs.sp(), 0x0300_7F00, "with the interrupted stack pointer");
    }

    /// A block transfer whose condition fails moves nothing, its base included.
    #[test]
    fn a_block_transfer_that_does_not_happen_leaves_its_base_alone() {
        // STMEQIA r4!, {r0-r2} with Z clear.
        let (mut cpu, mut mem) = machine(&[0x08A4_0007]);
        cpu.regs.set(4, DATA);
        cpu.regs.set_z(false);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(4), DATA);
        assert_eq!(mem.read32(DATA), 0, "and wrote nothing");
    }

    /// All sixteen at once, which is what a context switch is made of.
    #[test]
    fn all_sixteen_registers_go_out_and_come_back() {
        // STMIA r4, {r0-r15} then LDMIA r4, {r0-r14}
        let (mut cpu, mut mem) = machine(&[0xE884_FFFF]);
        for index in 0..15 {
            cpu.regs.set(index, 0x100 + index as u32);
        }
        cpu.regs.set(4, DATA);
        cpu.step(&mut mem).unwrap();

        for index in 0..15 {
            let wanted = if index == 4 { DATA } else { 0x100 + index as u32 };
            assert_eq!(mem.read32(DATA + index as u32 * 4), wanted, "r{index}");
        }
        assert_eq!(mem.read32(DATA + 60), BASE + 12, "and the counter, twelve on");
    }

    /// The register fields of a multiply are in different places from every
    /// other instruction's. Reading them by the usual positions gets four
    /// registers, all of them wrong, and an answer that still looks plausible.
    #[test]
    fn a_multiply_reads_its_registers_from_the_places_it_keeps_them() {
        // MUL r0, r1, r2  -  Rd is at 19..16, where an operand normally lives.
        let (mut cpu, mut mem) = machine(&[0xE000_0291]);
        cpu.regs.set(1, 6);
        cpu.regs.set(2, 7);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 42);
    }

    /// The low half of a product is the same whichever way the operands are
    /// read, so a plain multiply needs no notion of sign.
    #[test]
    fn a_multiply_wraps_and_does_not_care_about_sign() {
        // MUL r0, r1, r2 with two negatives.
        let (mut cpu, mut mem) = machine(&[0xE000_0291]);
        cpu.regs.set(1, 0xFFFF_FFFF);
        cpu.regs.set(2, 0xFFFF_FFFF);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 1, "-1 times -1");

        let (mut cpu, mut mem) = machine(&[0xE000_0291]);
        cpu.regs.set(1, 0x1234_5678);
        cpu.regs.set(2, 0x1000);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0x4567_8000, "and the top of the product falls off");
    }

    #[test]
    fn a_multiply_can_add_something_to_its_product() {
        // MLA r0, r1, r2, r3 - the destination at 19..16 and the addend at
        // 15..12, which is the trap the test above is about.
        let (mut cpu, mut mem) = machine(&[0xE020_3291]);
        cpu.regs.set(1, 6);
        cpu.regs.set(2, 7);
        cpu.regs.set(3, 100);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 142);
    }

    /// Two flags and not four. The architecture leaves the carry holding a
    /// meaningless value, so it is left alone rather than given an invented
    /// rule - a game cannot depend on what is not defined.
    #[test]
    fn a_multiply_with_flags_sets_two_of_them_and_leaves_the_rest() {
        // MULS r0, r1, r2
        let (mut cpu, mut mem) = machine(&[0xE010_0291]);
        cpu.regs.set(1, 0);
        cpu.regs.set(2, 5);
        cpu.regs.set_c(true);
        cpu.regs.set_v(true);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.z(), "the answer was zero");
        assert!(cpu.regs.c(), "the carry was not touched");
        assert!(cpu.regs.v(), "nor the overflow");

        let (mut cpu, mut mem) = machine(&[0xE010_0291]);
        cpu.regs.set(1, 0xFFFF_FFFF);
        cpu.regs.set(2, 1);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.n(), "and a negative answer sets the sign");
    }

    /// A long multiply keeps the whole product across two registers, and here
    /// the signedness matters because the sign reaches the top half.
    #[test]
    fn a_long_multiply_keeps_the_whole_product_and_minds_the_sign() {
        // UMULL r0, r1, r2, r3  -  low in r0, high in r1.
        let (mut cpu, mut mem) = machine(&[0xE081_0392]);
        cpu.regs.set(2, 0xFFFF_FFFF);
        cpu.regs.set(3, 0xFFFF_FFFF);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 1, "unsigned: the low half");
        assert_eq!(cpu.regs.get(1), 0xFFFF_FFFE, "and a very large top half");

        // SMULL r0, r1, r2, r3 - the same operands read as -1 each.
        let (mut cpu, mut mem) = machine(&[0xE0C1_0392]);
        cpu.regs.set(2, 0xFFFF_FFFF);
        cpu.regs.set(3, 0xFFFF_FFFF);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 1, "signed: -1 times -1 is one");
        assert_eq!(cpu.regs.get(1), 0, "with nothing in the top half at all");
    }

    #[test]
    fn a_long_multiply_can_accumulate_across_both_halves() {
        // UMLAL r0, r1, r2, r3
        let (mut cpu, mut mem) = machine(&[0xE0A1_0392]);
        cpu.regs.set(0, 0xFFFF_FFFF);
        cpu.regs.set(1, 0);
        cpu.regs.set(2, 2);
        cpu.regs.set(3, 1);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 1, "the low half wrapped");
        assert_eq!(cpu.regs.get(1), 1, "and carried into the high one");
    }

    /// The sign is the top bit of the whole 64-bit answer and the zero is both
    /// halves being zero, so neither flag can be read off one register.
    #[test]
    fn a_long_multiplys_flags_come_from_both_halves_at_once() {
        // SMULLS r0, r1, r2, r3 with a zero answer.
        let (mut cpu, mut mem) = machine(&[0xE0D1_0392]);
        cpu.regs.set(2, 0);
        cpu.regs.set(3, 1234);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.z(), "both halves were zero");

        // A product whose low half is zero but whose high half is not.
        let (mut cpu, mut mem) = machine(&[0xE0D1_0392]);
        cpu.regs.set(2, 0x1_0000);
        cpu.regs.set(3, 0x1_0000);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 0, "the low half is zero");
        assert_eq!(cpu.regs.get(1), 1);
        assert!(!cpu.regs.z(), "and the answer is not");

        // And a negative one.
        let (mut cpu, mut mem) = machine(&[0xE0D1_0392]);
        cpu.regs.set(2, 0xFFFF_FFFF);
        cpu.regs.set(3, 2);
        cpu.step(&mut mem).unwrap();
        assert!(cpu.regs.n(), "-2 is negative in sixty-four bits too");
    }

    /// The three instructions sharing this corner are told apart by bits that
    /// mean nothing anywhere else, so each has to reach its own.
    #[test]
    fn the_multiplies_and_the_swap_are_not_confused_with_each_other() {
        // A swap must not multiply.
        let (mut cpu, mut mem) = machine(&[0xE101_2090]);
        mem.write32(DATA, 0x1111);
        cpu.regs.set(0, 0x2222);
        cpu.regs.set(1, DATA);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(2), 0x1111, "it swapped");

        // And a multiply must not swap.
        let (mut cpu, mut mem) = machine(&[0xE000_0291]);
        cpu.regs.set(1, 3);
        cpu.regs.set(2, 4);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.get(0), 12, "it multiplied");
    }

    /// `SWI` is an exception raised on purpose: the status register is saved,
    /// the mode changes, interrupts are masked and the processor jumps to a
    /// fixed address.
    #[test]
    fn a_software_interrupt_goes_through_the_same_door_as_any_exception() {
        // SWI 0x060000 - a division, as it happens, though nothing here reads
        // the number.
        let (mut cpu, mut mem) = machine(&[0xEF06_0000]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !crate::cpu::registers::I);
        cpu.regs.set(13, 0x0300_7F00);
        let caller = cpu.regs.cpsr();

        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.pc(), 0x08, "it jumped to the vector");
        assert_eq!(cpu.regs.mode(), Mode::Supervisor, "in the mode with the rights");
        assert_eq!(cpu.regs.spsr(), caller, "with the caller's status saved");
        assert_eq!(cpu.regs.lr(), BASE + 4, "and the way back just past the instruction");
        assert!(cpu.regs.irq_disabled(), "interrupts masked until the handler has a stack");
        assert!(!cpu.regs.thumb(), "and in ARM state whatever the caller was doing");
    }

    /// The link register has to point *past* the instruction, because the
    /// handler fetches it back out of memory to see which service was asked
    /// for. The processor itself never reads those twenty-four bits.
    #[test]
    fn the_handler_can_find_the_number_the_caller_asked_for() {
        let (mut cpu, mut mem) = machine(&[0xEF12_3456]);
        cpu.step(&mut mem).unwrap();

        let instruction = mem.read32(cpu.regs.lr() - 4);
        assert_eq!(instruction & 0x00FF_FFFF, 0x12_3456, "the message came back out");
    }

    /// And it returns the way every other handler does.
    #[test]
    fn a_software_interrupt_comes_back_from_where_it_went() {
        // SWI, then at the vector: MOVS pc, lr
        let (mut cpu, mut mem) = machine(&[0xEF00_0000]);
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set(13, 0x0300_7F00);

        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.mode(), Mode::Supervisor);

        // The vector is in the BIOS, which cannot be written, so the handler is
        // run by hand from where it would have been.
        let (mut handler, mut mem2) = machine(&[0xE1B0_F00E]);
        handler.regs = cpu.regs.clone();
        handler.regs.set_pc(BASE);
        handler.step(&mut mem2).unwrap();

        assert_eq!(handler.regs.pc(), BASE + 4, "back to just past the SWI");
        assert_eq!(handler.regs.mode(), Mode::User, "and out of the handler's mode");
        assert_eq!(handler.regs.sp(), 0x0300_7F00, "with the caller's stack");
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
