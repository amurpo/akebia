# Akebia — a Game Boy / Game Boy Color emulator in Rust

A Game Boy and Game Boy Color emulator. The CPU passes Blargg's complete
`cpu_instrs` suite; the PPU draws background, window and sprites, in the DMG's
four shades or the CGB's 15-bit colour. The core is kept apart from the
frontend, so adding a new one —another GUI, WebAssembly— does not force any
changes to it.

```
akebia/
├── crates/
│   ├── akebia-core/     # pure emulation, no I/O and no dependencies
│   └── akebia-frontend/ # adapters: window, terminal, video-less commands
├── packaging/           # Fedora packaging
└── Cargo.toml           # workspace
```

## Usage

Build once and call the binary directly:

```bash
cargo build --release
./target/release/akebia path/to/game.gb
```

To have it on the `PATH` like any other command:

```bash
cargo install --path crates/akebia-frontend
akebia path/to/game.gb
```

```bash
akebia                           # list: pick a ROM from the last folder used
akebia --roms path/to/roms       # ...or from this other one
akebia game.gb                   # native window, ×4 scale (default)
akebia --scale 6 game.gb         # bigger; --scale 0 fits the screen
akebia --grayscale game.gb       # greys instead of the DMG greens
akebia --dmg game.gbc            # force monochrome on a colour game
akebia --cgb game.gb             # force colour mode
akebia --tui game.gb             # draw inside the terminal (no keyboard)
akebia --info game.gb            # cartridge metadata
akebia --trace 100 game.gb       # dump of instructions and registers
akebia --dump 900 game.gb        # dump a frame as ASCII, redirectable
akebia --dump 900 --ppm f.ppm g.gb   # ...or as a colour image
akebia --dump 900 --wav f.wav g.gb   # ...or the generated audio
akebia --serial test.gb          # dump the serial port (Blargg tests)
akebia --mute game.gb            # no sound
akebia --save other.sav game.gb  # saved game at another path
akebia --no-save game.gb         # do not load or write the saved game
akebia --debug game.gb           # PPU registers line by line
cargo test                   # 292 tests
```

### The game list

With no path, Akebia opens a list of the ROMs in a folder. It is walked with the
arrows or the mouse, there is a search box that filters as you type, and a game
starts with Enter or a double click. `Escape` leaves the game and returns to the
list.

Which folder it opens, in order: the one picked in **File → ROM folder…**, the
one from `--roms`, the last one a game was opened from, and failing all of those
one of the usual places (`roms`, `~/Juegos`, `~/Games`, `~/ROMs`,
`~/.local/share/akebia/roms`). Opening a ROM with "Open with Akebia" from the
file manager counts as using its folder too. Whichever way it was reached, the
folder is stored in `$XDG_STATE_HOME/akebia/last-folder`, and deleting that file
loses nothing but the convenience.

Guessing gets it wrong often enough —a collection under any other name, Akebia
run from the desktop launcher rather than from its own directory— so **the
folder is picked from the interface**, in the dialog that File → ROM folder…
opens; the path shown at the top right of the list opens the same dialog. Beside
each subfolder goes the number of ROMs in it, which is what turns a column of
names into a way of finding the collection, and the accept button carries the
count for the folder currently open, so there is no need to accept it to find
out whether it was the right one. The path box at the top takes a typed or
pasted path —`~` included— and is the way into a hidden folder or a mounted
drive, which the rows deliberately leave out.

The dialog is Akebia's own rather than the system file chooser. `rfd`, the usual
crate for that, wants GTK on Linux to compile or the desktop portal and an async
runtime to work, and either one undoes what `eframe` was chosen for: a binary
that runs wherever it is copied.

To the right of each game goes its mapper, and Game Boy Color exclusives carry a
badge too. **Only the header** of each file is read —336 bytes— and not the whole
ROM, so that a folder with dozens of multi-megabyte cartridges appears instantly.

A ROM that cannot be loaded —a mapper not implemented yet— does not close the
program: the error appears at the bottom and the list stays open. That is what is
needed when Akebia was opened from the desktop launcher, where there is no
terminal to read an error in.

### The menu

`File`, `Video` and `Audio` sit above both screens. It is where everything that
is neither "pick a game" nor "play" belongs: the ROM folder, the window size, the
palette monochrome games get and the sound. The command line only **seeds** those
settings; from there on the menu is what says how Akebia is set up, and a setting
survives leaving a game for the list and starting another one.

That is the point of having a menu at all with so few entries in it. Anything
added later —filters, forcing a console model, remapping the keys— is one more
line in a menu instead of another button wedged into a screen that was designed
around a list of games.

### Diagnosing graphics glitches

A rendering bug is not visible in the frame: it is visible in **what changed
between one line and the next**. The screen is drawn line by line reading the
registers as they stand at that instant, so a corrupt band halfway down means
something changed at exactly that height.

`--debug` dumps to stderr, while playing, the registers each line was drawn with.
Only the lines where something changes show up, and a frame identical to the
previous one is not repeated: it is counted. That way the output stays quiet
while nothing happens:

```
── frame 365 · 144 lines · HDMA 0 blocks
    LY  LCDC          SCX  SCY   WX   WY  WLN  VBK  HDMA
     0  87 LmwtbHOP    0    0    0    0    0    0     0
    16  E7 LMWtbHOP    0    0    0    0    0    0     0
```

There the window turns on at line 16: the `W` bit of `LCDC` goes uppercase. Each
letter is a bit of the register, uppercase if it is 1.

But a band of **wrong tiles** is not diagnosed that way: there the registers are
constant and the trace says nothing. For that, with `--debug` the **`D`** key
dumps the frame and the VRAM that produced it, as images:

| File | What it answers |
|---|---|
| `frame.ppm` | what was seen on screen |
| `bg.ppm` | the whole 256×256 background map, with the visible area boxed |
| `window.ppm` | the window map, which on CGB is usually the menu |
| `tiles.ppm` | the 384 tiles of each bank, exactly as they sit in VRAM |

It is read by elimination. If the tiles in `tiles.ppm` already come out broken,
the fault is in how they reach VRAM —the mapper or the HDMA— and not in the
rendering. If the tiles are fine but `bg.ppm` places them wrong, the problem is
the map. And if `bg.ppm` is whole but `frame.ppm` is not, then it really is the
renderer or the scroll.

Alongside them go two traces that answer *who* left the VRAM that way, without
inferring it from the final state —which admits many different stories—:

| File | What it holds |
|---|---|
| `writes.txt` | every write to VRAM: address, value, bank, `LY`, mode and **the `PC` that ordered it**, with the stride relative to the previous write from the same routine |
| `io.txt` | the same for the I/O registers, with the video ones separated and named (`LCDC`, `WY`, `WX`, `SCX`…) |

The `PC` is what matters: it says which routine of the game writes, and the
sequence of addresses gives the real stride from one write to the next. They are
rings of the last 16384 writes, so they keep the stretch right before the
capture.

They come with a `.txt` holding the registers, the maps in hexadecimal, OAM and
the palettes, plus VRAM and OAM raw in case they have to be inspected byte by
byte. Everything piles up in the `akebia-debug/` folder, which is in the
`.gitignore`: deleting it whole leaves the project clean.

`--dump N --debug` writes the same capture without opening a window, on reaching
frame `N`. It is the way to diagnose without having to reach the scene by hand.

### Saved games

Cartridges with a battery save only in play mode, into a `.sav` next to the ROM
(`game.gb` → `game.sav`). It is loaded at start-up and written on exit, plus an
autosave every second: `--tui` is closed with Ctrl+C, which kills the process
without going through the orderly shutdown, and that autosave is the only thing
protecting the game there.

`--trace` and `--dump` do **not** touch the `.sav`, neither to read nor to write:
they are debugging tools and their output has to depend on the ROM alone.

If the `.sav` does not match the cartridge, the game is played without saving and
a warning goes to stderr, instead of overwriting it.

On cartridges with a **clock** (MBC3+RTC) the file carries 48 extra bytes behind
the SRAM: the clock's ten registers —live and latched— and a Unix timestamp. It
is BGB's and VBA's format, so `.sav` files are interchangeable with them. The
timestamp is what allows **the clock to keep running with the emulator closed**:
on loading, the seconds elapsed since the save are added to it. A halted clock
does not advance, and a timestamp from the future —a system clock running behind,
or a `.sav` brought from another machine— is ignored instead of subtracting time.

### Controls

| Key | Button |
|---|---|
| Arrows | D-pad |
| `Z` / `X` | A / B |
| `Enter` / `Backspace` | Start / Select |
| `Escape` | Back to the list |
| `D` | Debug capture (with `--debug` only) |

In the list, the arrows move the cursor, `Home` and `End` go to the ends, and
`Enter` or a double click starts the game. In the folder dialog the arrows and
`Enter` do the same thing, `Backspace` goes up one folder and `Escape` cancels;
while it is open the game underneath is paused, so nothing typed into the path
box reaches the joypad.

### About the window

The interface is `eframe`/`egui`: pure Rust, with no toolkit that has to be
installed on the machine running the program. An emulator only asks to stretch a
160×144 texture and check the state of eight keys; GTK or Qt give far more than
that and in exchange complicate the RPM and make the Windows story an uphill
climb. With `eframe` a standalone binary still comes out and the same interface
compiles on Linux, Windows and macOS.

The `glow` backend —OpenGL— is used instead of the `wgpu` `eframe` ships by
default: for a 160×144 texture it is more than enough, it weighs considerably
less and it does not demand Vulkan or DX12 on the machine that runs it. `winit`
does negotiate client-side decorations, so Wayland works natively and with a
title bar; there is no longer any need to force X11 as with `minifb`.

The screen is drawn with nearest neighbour and **integer scaling**: a Game Boy
pixel comes out as an exact N×N square. With a fractional factor, some rows of
pixels would come out one thickness and others another.

The emulation has **no thread of its own**, and does not need one: a frame costs
less than a millisecond, so it fits inside the interface's cycle. What it does do
is decouple from the screen refresh, which may be 60, 120 or 144 Hz while the
Game Boy runs at 59.73: instead of emulating "one frame per repaint" it emulates
as many as fit in the elapsed time, with a cap of four so that coming back from a
minimised window does not try to recover a whole minute at once.

The `--tui` mode draws with Unicode half blocks and 24-bit colour: each `▀`
character is two vertical pixels, so the screen takes 160×72 cells. It needs a
terminal with truecolor. It does not accept keyboard input yet.

## Packaging

For Fedora, a single command:

```bash
./packaging/build-rpm.sh
sudo dnf install target/rpmbuild/RPMS/x86_64/akebia-*.rpm
```

It leaves a package of about 600 KiB with the binary at `/usr/bin/akebia`, the
README, the menu entry and the icon in eight sizes.

The entry declares the MIME types `application/x-gameboy-rom` and
`application/x-gameboy-color-rom`, which **Fedora already ships registered**.
With them the file manager offers "Open with Akebia" on right-clicking a `.gb`.
With no arguments —when opened from the applications menu— Akebia shows its game
list, so the entry works both ways.

The icon's eight sizes are derived at packaging time from the
`data/icons/akebia.png` master, instead of storing them all in the repository;
that is why the script needs ImageMagick.

The binary is built with `cargo` **outside** `rpmbuild`, which only packages it.
It is simpler than building inside and avoids having to declare cargo's whole
dependency tree as `BuildRequires`.

`rpmbuild` detects `libasound` on its own because it is linked. **Everything else
it does not**: `winit` and `glutin` open whatever they need with `dlopen` at run
time depending on where the program runs, so it does not show up in the ELF —an
`ldd` on the binary only shows ALSA and libc— and without declaring it the
package would install something that does not start. The list comes from looking
at the library names left in the binary:

```bash
strings -a target/release/akebia | grep -oE 'lib[A-Za-z0-9_.-]+\.so(\.[0-9]+)*'
```

They are declared by **soname** and not by package name: `libEGL.so.1` and
`libGL.so.1` come from mesa or from libglvnd depending on the Fedora version, and
asking for the soname lets rpm work out who provides them. Both worlds go in, X11
and Wayland, because the backend is chosen at start-up from the session.

| Library | How it gets in |
|---|---|
| `libasound.so.2` | linked, automatic |
| `libX11.so.6`, `libX11-xcb.so.1` | `dlopen`, declared by hand |
| `libXcursor.so.1`, `libXi.so.6`, `libXrender.so.1` | `dlopen`, declared by hand |
| `libxkbcommon.so.0`, `libxkbcommon-x11.so.0` | `dlopen`, declared by hand |
| `libwayland-client.so.0`, `libwayland-egl.so.1` | `dlopen`, declared by hand |
| `libEGL.so.1`, `libGL.so.1` | `dlopen`, declared by hand |

## Architecture

```
            ┌──────────────────────────────────────────┐
            │              GameBoy (Facade)            │
            │   ┌─────┐   reads/writes  ┌───────────┐  │
            │   │ Cpu │ ──────────────► │ SystemBus │  │
            │   └─────┘   (each access  └─────┬─────┘  │
            │             advances 1 M-cycle) │        │
            │            ┌────────┬────────┬──▼────┐   │
            │            │  Ppu   │ Timer  │ Apu   │   │
            │            └────────┴────────┴───────┘   │
            │                     │                    │
            │              ┌──────▼───────┐            │
            │              │ dyn Mapper   │  Strategy  │
            │              └──────────────┘            │
            └──────────────────────────────────────────┘
                         │              ▲
              VideoOutput │              │ buttons
                          ▼              │
        ┌─────────────────┴──────────────┴──────────────┐
        │   FrameSink     TerminalVideo    LastFrame     │  akebia-frontend
        │ (egui/texture)     (ANSI)         (--dump)     │
        └───────────────────────────────────────────────┘
```

The three video adapters are interchangeable at run time precisely because the
core never met any of them. `FrameSink` does not draw: it only converts the frame
into the colour the texture wants, and `egui` stretches it.

The ROM list is split in two: `roms.rs` is discovery and folder memory —reading a
directory, pulling the mapper out of the header, counting what a subfolder holds,
remembering which folder it was— and does not know a window exists, so it is
tested without opening one. `app.rs` is the interface, and that is where
everything that knows about `egui` lives. The folder dialog is split the same
way, which is why walking folders has tests while nothing opens a window.

### Design decisions

**Bus-driven timing.** Every CPU `read`/`write` consumes one M-cycle *and
advances the PPU, the timer and the DMA at that very instant*, before the CPU
sees the data. The alternative —running the whole instruction and adding up its
cycles at the end— is simpler but fails the `mem_timing` tests and breaks the
games that write video registers mid-instruction. It is the hardest decision in
the project to reverse, which is why it is taken from the start.

**The CPU depends on a trait, not on the bus.** `cpu::Bus` makes it possible to
run the CPU against a flat 64 KiB memory (`FlatBus`) in the unit tests, with no
PPU or cartridge in the way, and to check the M-cycles each instruction consumes
along the way.

**The core does no I/O.** It opens no files, no windows, and touches no audio. It
takes bytes and produces a framebuffer of palette indices. The conversion to RGB
lives in the frontend (`ports::Palette`), so the same core serves DMG, CGB and
any palette.

**The mappers are interchangeable.** `cartridge::Mapper` (Strategy) plus a
factory in `Cartridge::load`. Adding MBC3 is writing one file and one line.

### Module map

| Module | Contents |
|---|---|
| `cpu/registers.rs` | register file, flags, opcode encoding |
| `cpu/alu.rs` | arithmetic, flags and `DAA` (isolated and tested) |
| `cpu/bitops.rs` | rotates, shifts and `BIT` |
| `cpu/execute.rs` | decoder structured by blocks |
| `cpu/prefix_cb.rs` | the 256 opcodes prefixed with `0xCB` |
| `cpu/interrupts.rs` | `IE`/`IF`, priorities, vectors |
| `model.rs` | DMG or CGB; propagated by value to the PPU and the bus |
| `bus/mod.rs` | memory map, cycle handout, paged WRAM, DMA, open bus |
| `bus/hdma.rs` | CGB VRAM transfers, general and per HBlank |
| `ppu/mod.rs` | mode state machine, registers, paged VRAM |
| `ppu/color.rs` | RGB555 and the palette RAM |
| `ppu/render.rs` | scanline rendering: background, window and sprites |
| `timer.rs` | 16-bit counter and edge detection |
| `joypad.rs`, `serial.rs` | input and link port |
| `cartridge/header.rs` | cartridge metadata |
| `cartridge/mapper/` | the four MBCs, plus the shared SRAM |

## Status

Working:

- **Complete CPU.** All 245 legal unprefixed opcodes and the 256 of the `0xCB`
  prefix. It includes the quirks: `EI`'s delay, the HALT bug, `DAA`, the `Z`
  forced to 0 in `RLCA`/`RRCA`/`RLA`/`RRA`, and `ADD SP,e8`'s flags computed on
  the low byte. The 11 undefined opcodes are reported as `Fault::Illegal`.
- Cartridge loading: complete header and four mappers, which cover practically
  the whole catalogue:
  - **ROM ONLY** — 32 KiB straight.
  - **MBC1** — both banking modes, including the advanced one that also moves the
    low ROM region.
  - **MBC3** — 7-bit bank and a **real-time clock** with latching, halt bit and a
    9-bit day counter.
  - **MBC5** — 9-bit bank (512 banks, 8 MiB), selectable bank 0 and rumble bit.

  They all share the SRAM (`mapper/ram.rs`): enable, banks and dump to `.sav` if
  there is a battery. On the MBC3 the **clock is persisted too**, with the
  timestamp that keeps it running between sessions.
- Timer: 16-bit counter, falling edges, reload delay.
- PPU: state machine of the 4 modes with a **variable mode 3 duration** —the fine
  scroll, the window and each sprite stretch it from 172 to 289 dots, and that
  time is taken away from HBlank—, `LY`/`LYC`, edge-triggered STAT interrupt,
  VRAM/OAM locking by mode, and **complete rendering**: background with scroll
  and wrapping of the 256×256 canvas, window with its own line counter, and
  sprites with flipping on both axes, 8×16 mode, transparency, priority against
  the background and the limit of 10 per line.
- **Game Boy Color mode**, chosen from the header or forced with `--dmg`/`--cgb`:
  - 15-bit palettes, 8 for background and 8 for sprites, with auto-increment.
  - Two VRAM banks and eight WRAM banks.
  - Tile attributes in bank 1: palette, bank, flipping on both axes and priority
    over the sprites.
  - Sprite priority by OAM order, with `OPRI` to go back to the DMG rule.
  - General and HBlank HDMA.
  - Double speed via `KEY1` + `STOP`, with the PPU in its own clock domain.
- Bus: complete map, echo RAM, byte-by-byte OAM DMA, open bus. The **GDMA freezes
  the CPU** while it copies, like the hardware: spreading it over M-cycles let
  the game change `VBK` halfway and split the map across the two banks. The
  HBlank one does go block by block, which is the right thing there.
- **APU**: all four channels (two square waves with sweep and envelope, the wave
  table one and the noise one with its LFSR), the 512 Hz frame sequencer, stereo
  mixing through `NR51`/`NR50` and a high-pass filter. Each channel tells a
  disabled DAC —which contributes no voltage— from a connected DAC with sample 0,
  which is the negative extreme; collapsing them saturated the output.
- **Native window with keyboard**, plus the terminal mode and the video-less
  commands.
- **Game list** with a search box and the folder remembered between sessions.
  Without it, opening Akebia from the desktop launcher demanded a path there is
  no way to type there.
- **Menu bar** with the ROM folder, the window size, the palette and the sound.
  Guessing the folder was wrong often enough, and an empty list was a dead end:
  with no game to open, there was nothing for Akebia to learn the folder from.
- **Saved games** in `.sav`, with autosave and atomic writing.

The cartridges boot, reach their title screen and respond to the keyboard, both
Game Boy and Game Boy Color ones. With no limiter, the emulation runs at about 17
times real time.

### Validation

The CPU passes **Blargg's complete `cpu_instrs` suite**, all 11 tests:

```
$ akebia --dump 20000 --serial cpu_instrs.gb > /dev/null
cpu_instrs
01:ok  02:ok  03:ok  04:ok  05:ok  06:ok  07:ok  08:ok  09:ok  10:ok  11:ok
Passed all tests
```

The ROMs are downloaded from
[retrio/gb-test-roms](https://github.com/retrio/gb-test-roms). `--dump` with
`--serial` is the fastest way to run them: no window, no ANSI encoding, and the
ROM reports on stderr.

Pending, in the recommended order:

1. **Pixel FIFO**: it is the only thing that reproduces mid-line effects, and it
   means rewriting `render`. The variable mode 3 duration is already there; what
   is missing is validating both against `dmg-acid2` and `mooneye-gb`, which have
   to be downloaded.
2. **Keyboard in `--tui`**: the terminal mode draws but does not read input.

## Reference tests

- [Pan Docs](https://gbdev.io/pandocs/) — the hardware reference.
- [Blargg](https://github.com/retrio/gb-test-roms) — `cpu_instrs`,
  `instr_timing`, `mem_timing`. They report over the serial port.
- [dmg-acid2](https://github.com/mattcurrie/dmg-acid2) — the PPU in one frame.
- [mooneye-gb](https://github.com/Gekkio/mooneye-test-suite) — cycle accuracy;
  leave for last.

## Licence

GNU General Public License, version 3 or later. The full text is in
[COPYING](COPYING), and the RPM installs it alongside the binary.

The dependency tree allows it. Every crate in `Cargo.lock` is under a permissive
licence —MIT, Apache-2.0, BSD, Zlib, ISC and the like— and all of them combine
into GPLv3. Two are worth remembering when adding another dependency:
**Apache-2.0 goes one way only**, into GPLv3 and not back out, and `egui`'s
default fonts come under OFL-1.1 and the Ubuntu font licence, which cover the
font files rather than the program.

No ROM is covered by this licence or distributed here: `.gitignore` drops every
`.gb`, `.gbc` and `.sav`, so nothing from the `roms/` folder is ever committed.
The freely redistributable test suites are downloaded from their own projects,
listed above.
