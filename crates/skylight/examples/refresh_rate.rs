//! What every attached display says its refresh rate is.
//!
//! `CGDisplayModeGetRefreshRate` reports 0.0 for a panel whose timing the
//! window server drives itself rather than a mode line, so this exists to
//! show what a machine actually answers before anything caps a frame rate on
//! it. This one, a `Mac16,1`, answers 120.00 for its built-in display.

fn main() {
    for display in skylight::display::active().expect("the display list") {
        println!(
            "display {:>10}  builtin {:<5}  main {:<5}  {:>6.2} Hz",
            display.id(),
            display.is_builtin(),
            display.is_main(),
            display.refresh_rate(),
        );
    }
}
