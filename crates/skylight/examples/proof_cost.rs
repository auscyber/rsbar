//! What the main-thread check actually costs, since the whole arrangement is
//! only worth having if the answer is "nothing worth naming".
//!
//! Three shapes:
//!
//! 1. a plain call, for the baseline;
//! 2. the same call annotated, which takes proof and checks nothing;
//! 3. the runtime check this arrangement exists to not do, for scale.

use std::hint::black_box;
use std::time::Instant;

const ROUNDS: u32 = 10_000_000;

#[skylight::main_thread]
fn annotated(x: u64) -> u64 {
    black_box(proof);
    x + 1
}

fn plain(x: u64) -> u64 {
    x + 1
}

fn timed(what: &str, f: impl Fn() -> u64) {
    // Warm up, so the first call's lazy anything is not in the number.
    for _ in 0..10_000 {
        black_box(f());
    }
    let started = Instant::now();
    let mut total = 0u64;
    for _ in 0..ROUNDS {
        total = total.wrapping_add(f());
    }
    let elapsed = started.elapsed();
    black_box(total);
    let per_call = elapsed.as_secs_f64() * 1e9 / f64::from(ROUNDS);
    println!("{what:<34} {per_call:>8.2} ns/call");
}

fn main() {
    let mtm = objc2::MainThreadMarker::new().expect("an example runs on the main thread");
    timed("no proof at all", || plain(black_box(1)));
    timed("carrying proof", || {
        black_box(mtm);
        plain(black_box(1))
    });
    timed("#[main_thread] (takes proof)", || {
        annotated(black_box(mtm), black_box(1))
    });
    timed("a runtime check, for scale", || {
        black_box(objc2::MainThreadMarker::new()).map_or(0, |_| 1)
    });
}
