//! What a burst of updates actually costs on the main thread.
//!
//! The serial part of a pass is shaping the text of every item that changed
//! and then drawing once. Scripts already run on a worker pool, so this is
//! the part that concurrency could still buy something on — if it is slow.

use coolabah::components::Run;
use coolabah::layout::{BarPadding, Placed, arrange};
use coolabah::shaping::Shaped;
use coolabah_protocol::Position;
use coolabah_protocol::style::{Color, FontSpec};
use std::time::Instant;

fn run(text: &str) -> Run {
    Run {
        highlight: false,
        highlight_color: coolabah_protocol::style::Color::BLACK,
        y_offset: 0.0,
        string: text.to_owned(),
        font: FontSpec::default(),
        color: Color(0xffff_ffff),
        drawing: true,
        padding_left: 0.0,
        padding_right: 0.0,
    }
}

fn main() {
    for count in [10usize, 50, 100, 500] {
        // Every string distinct, so nothing is served from a cache.
        let labels: Vec<Run> = (0..count)
            .map(|i| run(&format!("item {i} 12:34:56")))
            .collect();
        let icon = run("");

        let spec = FontSpec::default();
        let started = Instant::now();
        let fonts: Vec<coolabah::text::Font> = (0..count)
            .map(|_| coolabah::text::Font::resolve(&spec))
            .collect();
        let resolving = started.elapsed();
        std::hint::black_box(&fonts);

        let started = Instant::now();
        let shaped: Vec<Shaped> = labels
            .iter()
            .map(|label| Shaped::new(&icon, label))
            .collect();
        let shaping = started.elapsed();

        let started = Instant::now();
        let measured: Vec<Placed<usize>> = shaped
            .iter()
            .enumerate()
            .map(|(i, s)| Placed {
                id: i,
                position: Position::Left,
                width: s.label_metrics().width,
            })
            .collect();
        let placed = arrange(&measured, 3840.0, BarPadding::default(), 0.0);
        let layout = started.elapsed();

        let n = u32::try_from(count).unwrap_or(1);
        println!(
            "{count:>4} items: resolve {:>8.2?}/item, shape {:>8.2?}/item, layout {layout:>9.2?}, placed {}",
            resolving / n,
            shaping / n,
            placed.len()
        );
    }
}
