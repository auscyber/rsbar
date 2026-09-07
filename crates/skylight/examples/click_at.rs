//! Posts a real mouse click at a screen point, for testing the bar's own
//! click handling. `cargo run -p skylight --example click_at -- X Y`

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};

fn main() {
    let mut args = std::env::args().skip(1);
    let x: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0.0);
    let y: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0.0);
    let at = CGPoint::new(x, y);

    for kind in [CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let Some(event) = CGEvent::new_mouse_event(None, kind, at, CGMouseButton::Left) else {
            eprintln!("could not create the event; posting needs Accessibility permission");
            std::process::exit(1);
        };
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    println!("clicked at ({x}, {y})");
}
