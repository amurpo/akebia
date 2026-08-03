Name:           akebia
Version:        %{ver}
Release:        1%{?dist}
Summary:        Game Boy and Game Boy Color emulator

License:        GPL-3.0-or-later
URL:            https://github.com/amurpo/akebia

# The binary arrives already built from build-rpm.sh, so nothing is compiled
# here and no BuildRequires are needed.

# `libasound` is linked and rpmbuild detects it on its own. Everything else is
# **not**: winit and glutin open whatever they need with dlopen at run time
# depending on where they run, so it does not show up in the ELF —`ldd` on the
# binary only shows ALSA and libc— and it has to be declared by hand or the
# package would install something that does not start.
#
# They are requested by soname and not by package name on purpose: `libEGL.so.1`
# and `libGL.so.1` come from mesa or from libglvnd depending on the Fedora
# version, and asking for the soname lets rpm work out who provides them.
#
# Both worlds go in, X11 and Wayland, because the backend is chosen at start-up
# from the session and the package does not know which one it will land in.
Requires:       libX11.so.6()(64bit)
Requires:       libX11-xcb.so.1()(64bit)
Requires:       libXcursor.so.1()(64bit)
Requires:       libXi.so.6()(64bit)
Requires:       libXrender.so.1()(64bit)
Requires:       libxkbcommon.so.0()(64bit)
Requires:       libxkbcommon-x11.so.0()(64bit)
Requires:       libwayland-client.so.0()(64bit)
Requires:       libwayland-egl.so.1()(64bit)
Requires:       libEGL.so.1()(64bit)
Requires:       libGL.so.1()(64bit)

%description
Game Boy (DMG) and Game Boy Color emulator written from scratch in Rust.

It includes a CPU validated against Blargg's cpu_instrs suite, a PPU with
background, window and sprites, the four most common mappers (ROM ONLY, MBC1,
MBC3 with clock, MBC5), sound over all four channels and saved games in .sav.

It runs in a native window, inside the terminal with --tui, or with no video at
all to dump frames and debug traces.

%prep
# There is no tarball to unpack: the binary already comes built from
# build-rpm.sh. The README and the licence are copied here because
# documentation is looked for in the build directory, not in the source one.
cp -a %{_sourcedir}/README.md .
cp -a %{_sourcedir}/COPYING .

%install
install -Dm755 %{_sourcedir}/akebia %{buildroot}%{_bindir}/akebia
install -Dm644 %{_sourcedir}/akebia.desktop \
               %{buildroot}%{_datadir}/applications/akebia.desktop

for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 %{_sourcedir}/akebia-${size}.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/akebia.png
done

%files
# `%%license` and not `%%doc`: it puts COPYING under /usr/share/licenses and
# keeps it installed even where documentation is excluded, which is what the
# GPL asks for — the terms have to travel with the binary.
%license COPYING
%{_bindir}/akebia
%{_datadir}/applications/akebia.desktop
%{_datadir}/icons/hicolor/*/apps/akebia.png
%doc README.md

# No scriptlets to refresh the icon cache or the application database: since
# Fedora 26 the system's own file triggers do it, and the packaging guidelines
# ask **not** to repeat it in every package.

%changelog
* Sun Aug 02 2026 Daniel Avila - %{ver}-1
- First packaging
