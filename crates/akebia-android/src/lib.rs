//! Android entry point.
//!
//! It is the counterpart of `akebia-frontend`'s `main.rs`: where that one is
//! handed a command line by a shell, this one is handed an `AndroidApp` by the
//! system. Both then ask the same library for the same window.
//!
//! # Why a crate of its own
//!
//! The entry point has to be an unmangled symbol —`NativeActivity` looks it up
//! by name inside the shared object— and unmangling one is an unsafe attribute
//! in the 2024 edition. `akebia-frontend` forbids unsafe outright, and that is
//! worth keeping: it is the crate where the emulator's every adapter lives. So
//! the `#[unsafe(no_mangle)]` is quarantined here, in twenty lines that do
//! nothing else.

// Away from a telephone all that is left is `pad`, which is where the controls
// are measured out. That is on purpose: it is arithmetic, it is the part most
// likely to come out wrong on a screen nobody tried, and built here it can be
// tested by `cargo test` like anything else.
pub mod pad;

#[cfg(target_os = "android")]
mod java;
#[cfg(target_os = "android")]
mod ui;

#[cfg(target_os = "android")]
use akebia_frontend::args::{Args, Parsed};
#[cfg(target_os = "android")]
use eframe::NativeOptions;
#[cfg(target_os = "android")]
use winit::platform::android::activity::AndroidApp;

#[cfg(target_os = "android")]
use java::Java;
#[cfg(target_os = "android")]
use ui::Phone;

/// What Android calls once the activity is up, on a thread of its own.
///
/// Returning from here ends the application, so it does not return until the
/// window is closed.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("akebia"),
    );

    // The defaults are read out of the parser instead of being written again
    // here: `--scale 4`, sound on and the rest live in `args.rs` and there is no
    // reason for a telephone to hold a second copy of them that can drift.
    // Neither the help nor an error can come out of an empty command line.
    let Ok(Parsed::Run(args)) = Args::parse(std::iter::empty::<String>()) else {
        return;
    };

    // The way through to the activity, which is the only one who can open the
    // system's file browser or say where this application's folder is.
    let java = Java::new(&app);

    // The one thing only this entry point can supply: `eframe` builds `winit`'s
    // event loop out of it, and it exists nowhere else.
    let options = NativeOptions { android_app: Some(app), ..Default::default() };

    let started = eframe::run_native(
        "Akebia",
        options,
        Box::new(move |cc| Ok(Box::new(Phone::new(cc, java, *args)))),
    );
    if let Err(message) = started {
        log::error!("the window could not be opened: {message}");
    }
}
