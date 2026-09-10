//! Scratch: dump every on-screen window across every layer, not just the
//! menu bar layer, to find where the Apple menu / app menu titles live.
use objc2_core_foundation::{CGRect, CGSize};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use skylight::cf::{Array, WindowKey};

fn main() {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).expect("window list");
    let list = Array::new(&list);
    println!("{} windows total", list.len());

    let mut rows = Vec::new();
    for i in 0..list.len() {
        let Some(dict) = list.dict(i) else {
            continue;
        };
        rows.push((
            dict.get::<i64>(WindowKey::Layer).unwrap_or(i64::MIN),
            dict.get::<String>(WindowKey::OwnerName).unwrap_or_default(),
            dict.get::<String>(WindowKey::Name).unwrap_or_default(),
            dict.get::<i64>(WindowKey::OwnerPid).unwrap_or(-1),
            dict.get::<i64>(WindowKey::Number).unwrap_or(-1),
            dict.get::<CGRect>(WindowKey::Bounds).unwrap_or(CGRect::new(
                objc2_core_foundation::CGPoint::new(0.0, 0.0),
                CGSize::new(0.0, 0.0),
            )),
        ));
    }
    rows.sort_by_key(|r| r.0);
    for (layer, owner, name, pid, id, bounds) in &rows {
        println!(
            "layer={layer:<6} id={id:<6} owner={owner:<28?} name={name:<28?} pid={pid:<8} bounds={bounds:?}"
        );
    }
}
