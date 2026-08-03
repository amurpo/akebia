//! CPU tests against [`FlatBus`], with no PPU or timer in the way.
//!
//! Every test checks two things: the effect on the registers **and** the
//! M-cycles consumed. The second one is usually what is wrong.

use super::*;

/// Sets up a CPU with `PC = 0xC000` (WRAM) and the program already loaded.
fn boot(program: &[u8]) -> (Cpu, FlatBus) {
    let mut cpu = Cpu::new_dmg();
    cpu.regs.pc = 0xC000;
    cpu.regs.sp = 0xFFFE;
    let mut bus = FlatBus::new();
    bus.load(0xC000, program);
    (cpu, bus)
}

/// Executes one instruction and returns the M-cycles the bus consumed.
fn step(cpu: &mut Cpu, bus: &mut FlatBus) -> u32 {
    bus.reset_cycles();
    let declared = cpu.step(bus).expect("the instruction must execute");
    assert_eq!(
        declared, bus.cycles,
        "the declared M-cycles do not match the ones the bus consumed"
    );
    declared
}

#[test]
fn nop_advances_the_pc_and_costs_one_cycle() {
    let (mut cpu, mut bus) = boot(&[0x00]);
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.regs.pc, 0xC001);
}

#[test]
fn ld_between_registers() {
    // LD B,A
    let (mut cpu, mut bus) = boot(&[0x47]);
    cpu.regs.a = 0x42;
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.regs.b, 0x42);
}

#[test]
fn ld_through_hl_costs_one_more_cycle() {
    // LD A,(HL)
    let (mut cpu, mut bus) = boot(&[0x7E]);
    cpu.regs.set_hl(0xD000);
    bus.memory[0xD000] = 0x99;
    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(cpu.regs.a, 0x99);
}

#[test]
fn ld_hl_increment_moves_the_pointer() {
    // LD (HL+),A
    let (mut cpu, mut bus) = boot(&[0x22]);
    cpu.regs.a = 0x7B;
    cpu.regs.set_hl(0xD000);
    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(bus.memory[0xD000], 0x7B);
    assert_eq!(cpu.regs.hl(), 0xD001);
}

#[test]
fn addition_with_an_immediate_operand() {
    // ADD A,0x0F
    let (mut cpu, mut bus) = boot(&[0xC6, 0x0F]);
    cpu.regs.a = 0x01;
    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(cpu.regs.a, 0x10);
    assert!(cpu.regs.f.h());
}

#[test]
fn backwards_relative_jump() {
    // JR -2: infinite loop onto itself, the classic wait idiom.
    let (mut cpu, mut bus) = boot(&[0x18, 0xFE]);
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(cpu.regs.pc, 0xC000);
}

#[test]
fn an_untaken_conditional_jump_costs_less() {
    // JR Z,+4 with Z = 0
    let (mut cpu, mut bus) = boot(&[0x28, 0x04]);
    cpu.regs.f = Flags::from_bits(0);
    assert_eq!(step(&mut cpu, &mut bus), 2, "without the jump it is 2 M-cycles");
    assert_eq!(cpu.regs.pc, 0xC002);

    cpu.regs.pc = 0xC000;
    cpu.regs.f.set(Flags::Z, true);
    assert_eq!(step(&mut cpu, &mut bus), 3, "with the jump it is 3");
    assert_eq!(cpu.regs.pc, 0xC006);
}

#[test]
fn call_and_ret_preserve_the_return_address() {
    // CALL 0xD000
    let (mut cpu, mut bus) = boot(&[0xCD, 0x00, 0xD0]);
    bus.load(0xD000, &[0xC9]); // RET

    assert_eq!(step(&mut cpu, &mut bus), 6);
    assert_eq!(cpu.regs.pc, 0xD000);
    assert_eq!(cpu.regs.sp, 0xFFFC);
    assert_eq!(bus.memory[0xFFFC], 0x03, "low byte of the return address");
    assert_eq!(bus.memory[0xFFFD], 0xC0, "high byte of the return address");

    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert_eq!(cpu.regs.pc, 0xC003);
    assert_eq!(cpu.regs.sp, 0xFFFE);
}

#[test]
fn push_and_pop_discard_the_low_bits_of_f() {
    // PUSH BC ; POP AF
    let (mut cpu, mut bus) = boot(&[0xC5, 0xF1]);
    cpu.regs.write16(R16::Bc, 0x12FF);

    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(cpu.regs.read16(R16::Af), 0x12F0);
}

#[test]
fn rst_jumps_to_the_fixed_vector() {
    // RST 0x38
    let (mut cpu, mut bus) = boot(&[0xFF]);
    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert_eq!(cpu.regs.pc, 0x0038);
}

#[test]
fn ldh_reaches_the_io_page() {
    // LDH (0x47),A — the BGP palette register
    let (mut cpu, mut bus) = boot(&[0xE0, 0x47]);
    cpu.regs.a = 0xE4;
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(bus.memory[0xFF47], 0xE4);
}

#[test]
fn inc_in_memory_costs_three_cycles() {
    // INC (HL)
    let (mut cpu, mut bus) = boot(&[0x34]);
    cpu.regs.set_hl(0xD000);
    bus.memory[0xD000] = 0xFF;
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(bus.memory[0xD000], 0x00);
    assert!(cpu.regs.f.z() && cpu.regs.f.h());
}

// ---- Interrupts ------------------------------------------------------------

#[test]
fn ei_does_not_take_effect_until_the_next_instruction() {
    // EI ; NOP
    let (mut cpu, mut bus) = boot(&[0xFB, 0x00]);
    step(&mut cpu, &mut bus);
    assert!(!cpu.ime(), "IME is still off right after EI");
    step(&mut cpu, &mut bus);
    assert!(cpu.ime(), "IME turns on after the following instruction");
}

#[test]
fn ei_followed_by_di_never_enables() {
    // EI ; DI
    let (mut cpu, mut bus) = boot(&[0xFB, 0xF3]);
    step(&mut cpu, &mut bus);
    step(&mut cpu, &mut bus);
    assert!(!cpu.ime());
}

#[test]
fn the_dispatch_pushes_the_pc_and_jumps_to_the_vector() {
    // EI ; NOP ; (the interrupt lands here)
    let (mut cpu, mut bus) = boot(&[0xFB, 0x00, 0x00]);
    bus.interrupts.write_enable(Interrupt::Timer.mask());

    step(&mut cpu, &mut bus); // EI
    step(&mut cpu, &mut bus); // NOP, after which IME becomes active
    assert!(cpu.ime());

    bus.interrupts.request(Interrupt::Timer);
    assert_eq!(step(&mut cpu, &mut bus), 5, "servicing an interrupt costs 5 M-cycles");

    assert_eq!(cpu.regs.pc, Interrupt::Timer.vector());
    assert!(!cpu.ime(), "the dispatch turns IME off");
    assert_eq!(bus.interrupts.pending(), None, "the IF bit was cleared");
    assert_eq!(cpu.regs.sp, 0xFFFC);
    assert_eq!(bus.memory[0xFFFD], 0xC0, "high byte of the pushed PC");
    assert_eq!(bus.memory[0xFFFC], 0x02, "low byte of the pushed PC");
}

#[test]
fn halt_resumes_on_a_pending_interrupt() {
    // HALT
    let (mut cpu, mut bus) = boot(&[0x76]);
    bus.interrupts.write_enable(0);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.power, Power::Halted);

    // With no interrupts enabled it stays asleep and burns cycles.
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.power, Power::Halted);

    bus.interrupts.write_enable(Interrupt::VBlank.mask());
    bus.interrupts.request(Interrupt::VBlank);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.power, Power::Running, "a pending interrupt wakes the CPU up");
}

#[test]
fn the_halt_bug_repeats_the_next_byte() {
    // HALT ; INC A  — with IME=0 and a pending interrupt, INC A runs twice
    // because the PC does not advance after the fetch.
    let (mut cpu, mut bus) = boot(&[0x76, 0x3C]);
    cpu.regs.a = 0x00;
    bus.interrupts.write_enable(Interrupt::VBlank.mask());
    bus.interrupts.request(Interrupt::VBlank);

    step(&mut cpu, &mut bus); // HALT: does not sleep, it arms the bug
    assert_eq!(cpu.power, Power::Running);

    step(&mut cpu, &mut bus); // first INC A, the PC does not advance
    assert_eq!(cpu.regs.a, 0x01);
    assert_eq!(cpu.regs.pc, 0xC001, "the PC stayed on the same opcode");

    step(&mut cpu, &mut bus); // second INC A
    assert_eq!(cpu.regs.a, 0x02);
    assert_eq!(cpu.regs.pc, 0xC002);
}

// ---- Faults ----------------------------------------------------------------

#[test]
fn illegal_opcodes_are_reported() {
    let (mut cpu, mut bus) = boot(&[0xD3]);
    assert_eq!(cpu.step(&mut bus), Err(Fault::Illegal { opcode: 0xD3, pc: 0xC000 }));
}

/// The 11 opcodes the SM83 does not define, per the Pan Docs table.
const ILLEGAL: [u8; 11] = [0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD];

#[test]
fn no_opcode_is_left_unimplemented() {
    for op in 0x00..=0xFFu8 {
        let (mut cpu, mut bus) = boot(&[op, 0x00, 0x00]);
        // SP in the middle of RAM: PUSH and CALL must not run off the end.
        cpu.regs.sp = 0xDF00;

        match cpu.step(&mut bus) {
            Ok(_) => {}
            Err(Fault::Illegal { opcode, .. }) => {
                assert!(ILLEGAL.contains(&opcode), "0x{op:02X} is not illegal");
            }
            Err(fault) => panic!("opcode 0x{op:02X} unimplemented: {fault:?}"),
        }
    }
}

#[test]
fn the_cb_prefix_is_complete() {
    for op in 0x00..=0xFFu8 {
        let (mut cpu, mut bus) = boot(&[0xCB, op]);
        cpu.regs.set_hl(0xD000);
        assert!(cpu.step(&mut bus).is_ok(), "prefixed opcode 0xCB 0x{op:02X} unimplemented");
    }
}

#[test]
fn every_illegal_opcode_is_declared() {
    for op in ILLEGAL {
        let (mut cpu, mut bus) = boot(&[op]);
        assert!(matches!(cpu.step(&mut bus), Err(Fault::Illegal { .. })));
    }
}

// ---- CB prefix -------------------------------------------------------------

#[test]
fn cb_rotates_a_register_in_two_cycles() {
    // RLC B
    let (mut cpu, mut bus) = boot(&[0xCB, 0x00]);
    cpu.regs.b = 0b1000_0001;
    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(cpu.regs.b, 0b0000_0011);
    assert!(cpu.regs.f.c());
}

#[test]
fn cb_on_hl_costs_four_cycles() {
    // SWAP (HL)
    let (mut cpu, mut bus) = boot(&[0xCB, 0x36]);
    cpu.regs.set_hl(0xD000);
    bus.memory[0xD000] = 0xAB;
    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert_eq!(bus.memory[0xD000], 0xBA);
}

#[test]
fn bit_on_hl_costs_three_because_it_does_not_write() {
    // BIT 7,(HL)
    let (mut cpu, mut bus) = boot(&[0xCB, 0x7E]);
    cpu.regs.set_hl(0xD000);
    bus.memory[0xD000] = 0x80;
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert!(!cpu.regs.f.z(), "bit 7 is on");
    assert_eq!(bus.memory[0xD000], 0x80, "BIT does not modify the operand");
}

#[test]
fn res_and_set_manipulate_the_given_bit() {
    // RES 3,A ; SET 5,A
    let (mut cpu, mut bus) = boot(&[0xCB, 0x9F, 0xCB, 0xEF]);
    cpu.regs.a = 0xFF;

    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(cpu.regs.a, 0b1111_0111);

    cpu.regs.a = 0x00;
    assert_eq!(step(&mut cpu, &mut bus), 2);
    assert_eq!(cpu.regs.a, 0b0010_0000);
}

// ---- Accumulator operations ------------------------------------------------

#[test]
fn rlca_does_not_set_z_but_rlc_a_does() {
    // RLCA with A = 0
    let (mut cpu, mut bus) = boot(&[0x07]);
    cpu.regs.a = 0x00;
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert!(!cpu.regs.f.z(), "RLCA forces Z to 0");

    // The prefixed twin, CB 07 (RLC A), on the same value.
    let (mut cpu, mut bus) = boot(&[0xCB, 0x07]);
    cpu.regs.a = 0x00;
    step(&mut cpu, &mut bus);
    assert!(cpu.regs.f.z(), "RLC A does evaluate Z");
}

#[test]
fn rla_chains_the_carry_between_bytes() {
    // RL C ; RLA — the idiom for shifting a 16-bit value in CA.
    let (mut cpu, mut bus) = boot(&[0xCB, 0x11, 0x17]);
    cpu.regs.a = 0b0000_0001;
    cpu.regs.c = 0b1000_0000;
    cpu.regs.f = Flags::from_bits(0);

    step(&mut cpu, &mut bus); // RL C: bit 7 goes out to the carry
    assert_eq!(cpu.regs.c, 0x00);
    assert!(cpu.regs.f.c());

    step(&mut cpu, &mut bus); // RLA: the carry enters through bit 0 of A
    assert_eq!(cpu.regs.a, 0b0000_0011);
}

#[test]
fn daa_after_a_bcd_addition() {
    // ADD A,B ; DAA
    let (mut cpu, mut bus) = boot(&[0x80, 0x27]);
    cpu.regs.a = 0x19; // 19 in BCD
    cpu.regs.b = 0x28; // 28 in BCD

    step(&mut cpu, &mut bus);
    assert_eq!(cpu.regs.a, 0x41, "the binary sum gives 0x41, which is not 19+28");
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.regs.a, 0x47, "DAA fixes it to 47");
}

#[test]
fn cpl_scf_and_ccf() {
    // CPL ; SCF ; CCF
    let (mut cpu, mut bus) = boot(&[0x2F, 0x37, 0x3F]);
    cpu.regs.a = 0x0F;

    step(&mut cpu, &mut bus);
    assert_eq!(cpu.regs.a, 0xF0);

    step(&mut cpu, &mut bus);
    assert!(cpu.regs.f.c());

    step(&mut cpu, &mut bus);
    assert!(!cpu.regs.f.c());
}

// ---- Stack pointer operations ----------------------------------------------

#[test]
fn ld_nn_sp_writes_both_bytes_in_little_endian() {
    // LD (0xD000),SP
    let (mut cpu, mut bus) = boot(&[0x08, 0x00, 0xD0]);
    cpu.regs.sp = 0xBEEF;
    assert_eq!(step(&mut cpu, &mut bus), 5);
    assert_eq!(bus.memory[0xD000], 0xEF);
    assert_eq!(bus.memory[0xD001], 0xBE);
}

#[test]
fn add_sp_accepts_negative_offsets() {
    // ADD SP,-2
    let (mut cpu, mut bus) = boot(&[0xE8, 0xFE]);
    cpu.regs.sp = 0xFFFE;
    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert_eq!(cpu.regs.sp, 0xFFFC);
    assert!(!cpu.regs.f.z(), "Z always ends up at 0");
}

#[test]
fn ld_hl_sp_plus_e8_does_not_touch_sp() {
    // LD HL,SP+8
    let (mut cpu, mut bus) = boot(&[0xF8, 0x08]);
    cpu.regs.sp = 0xFFF8;
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(cpu.regs.hl(), 0x0000);
    assert_eq!(cpu.regs.sp, 0xFFF8);
    assert!(cpu.regs.f.h() && cpu.regs.f.c(), "the flags come from the low byte");
}

#[test]
fn stop_halts_the_cpu_and_consumes_two_bytes() {
    // STOP
    let (mut cpu, mut bus) = boot(&[0x10, 0x00]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.power, Power::Stopped);
    assert_eq!(cpu.regs.pc, 0xC002, "STOP takes up two bytes");
}
