//! Milestone 2 demo: a bar with shaped text in it.

use objc2_core_foundation::{CFRunLoop, CGPoint, CGRect, CGSize, kCFRunLoopDefaultMode};
use objc2_core_graphics::{CGContext, CGDisplayBounds, CGMainDisplayID};
use rsbar::style::{Color, FontSpec};
use rsbar::text::{Font, Text};
use skylight::{Window, WindowTags, level};

const HEIGHT: f64 = 36.0;
const PADDING: f64 = 12.0;

fn main() {
    let bounds = CGDisplayBounds(CGMainDisplayID());
    let frame = CGRect::new(bounds.origin, CGSize::new(bounds.size.width, HEIGHT));

    let window = Window::new(frame).expect("create bar window");
    window.set_scale(2.0).expect("scale");
    window.set_opaque(false).expect("opacity");
    window.set_alpha(1.0).expect("alpha");
    window.set_level(level::STATUS).expect("level");
    window
        .set_tags((WindowTags::BAR - WindowTags::AVOIDS_CAPTURE) | WindowTags::IGNORE_FOR_EVENTS)
        .expect("tags");
    window.order_above(None).expect("order in");

    let icon_font = Font::resolve(&FontSpec::parse("Menlo:Bold:15"));
    let label_font = Font::resolve(&FontSpec::parse("Menlo:Regular:13"));

    let left = [
        (Text::new("\u{2318}", icon_font.clone()), Color::WHITE),
        (
            Text::new("rsbar", label_font.clone()),
            "#7fd1ff".parse().unwrap(),
        ),
    ];
    let right = [
        (
            Text::new("cpu 12%", label_font.clone()),
            "#b0b8c4".parse().unwrap(),
        ),
        (Text::new("09:41", label_font.clone()), Color::WHITE),
    ];

    let background: Color = "#e0141820".parse().unwrap();

    skylight::draw(window.id(), frame.size, |ctx| {
        CGContext::set_rgb_fill_color(
            Some(ctx),
            background.red(),
            background.green(),
            background.blue(),
            background.alpha(),
        );
        CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), frame.size));

        let mut x = PADDING;
        for (item, color) in &left {
            let w = item.metrics().width;
            item.draw(
                ctx,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, HEIGHT)),
                *color,
            );
            x += w + PADDING;
        }

        // Right bucket lays out right-to-left, so trailing items stay pinned.
        let mut x = frame.size.width - PADDING;
        for (item, color) in right.iter().rev() {
            let w = item.metrics().width;
            x -= w;
            item.draw(
                ctx,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, HEIGHT)),
                *color,
            );
            x -= PADDING;
        }
    });

    println!("bar {} up; holding 10s", window.id());
    // Composition only happens while a run loop runs.
    unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 10.0, false) };
}
