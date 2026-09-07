//! Scratch: dump every on-screen window across every layer, not just the
//! menu bar layer, to find where the Apple menu / app menu titles live.
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowBounds, kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};
use std::ffi::c_void;
use std::ptr::NonNull;

unsafe fn dict_at(array: &CFArray, i: isize) -> Option<&CFDictionary> {
    let ptr = unsafe { array.value_at_index(i) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    unsafe { ptr.as_ref() }.downcast_ref::<CFDictionary>()
}

fn cf_key(key: &CFString) -> *const c_void {
    (std::ptr::from_ref(key)).cast()
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    let ptr = unsafe { dict.value(cf_key(key)) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    Some(unsafe { ptr.as_ref() })
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    Some(
        dict_value(dict, key)?
            .downcast_ref::<CFString>()?
            .to_string(),
    )
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    dict_value(dict, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn main() {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).expect("window list");
    println!("{} windows total", list.count());
    let mut rows = Vec::new();
    for i in 0..list.count() {
        let Some(dict) = (unsafe { dict_at(&list, i) }) else {
            continue;
        };
        let layer = dict_i64(dict, unsafe { kCGWindowLayer }).unwrap_or(i64::MIN);
        let owner = dict_string(dict, unsafe { kCGWindowOwnerName }).unwrap_or_default();
        let name = dict_string(dict, unsafe { kCGWindowName }).unwrap_or_default();
        let pid = dict_i64(dict, unsafe { kCGWindowOwnerPID }).unwrap_or(-1);
        let id = dict_i64(dict, unsafe { kCGWindowNumber }).unwrap_or(-1);
        let bounds = dict_value(dict, unsafe { kCGWindowBounds })
            .and_then(|v| v.downcast_ref::<CFDictionary>())
            .and_then(|b| {
                let mut rect = objc2_core_foundation::CGRect::default();
                unsafe { CGRectMakeWithDictionaryRepresentation(Some(b), &raw mut rect) }
                    .then_some(rect)
            });
        rows.push((layer, owner, name, pid, id, bounds));
    }
    rows.sort_by_key(|r| r.0);
    for (layer, owner, name, pid, id, bounds) in &rows {
        println!(
            "layer={layer:<6} id={id:<6} owner={owner:<28?} name={name:<28?} pid={pid:<8} bounds={bounds:?}"
        );
    }
}
