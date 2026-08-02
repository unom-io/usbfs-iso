//! `iso-probe` — the hardware-facing half of this repo.
//!
//! Four things the library cannot do for you, because they need a real device on a real bus:
//!
//! - `list` / `dump` — see what is actually there, and capture a descriptor blob so the tier-0
//!   fixtures can be replaced with a real one.
//! - `spike` — **WP0.** Prove that an audio interface can be force-claimed away from
//!   `snd-usb-audio` while the rest of a composite device keeps working, and that a single
//!   isochronous URB completes. This is the twenty-minute check that greenlights or kills the
//!   whole approach on a given kernel.
//! - `sweep` — **WP7.** Measure the lowest underrun-free in-flight depth, across depth ×
//!   packets-per-URB × thread policy. The deliverable is a number.
//! - `tone` — play a sine, which is the only way to tell "no errors reported" from "actually
//!   sounds right".
//!
//! No argument-parsing dependency on purpose: this repo's whole pitch is that it drags nothing in.

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn main() {
    eprintln!(
        "iso-probe drives the Linux usbfs kernel ABI and only runs on Linux or Android.\n\
         The library's parsing and packet arithmetic are testable anywhere: cargo test --workspace"
    );
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod cli;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod spike;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod sweep;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod tone;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
