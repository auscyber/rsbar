//! Waking the main thread from a worker.
//!
//! Drawing, and every window server call behind it, belongs to the thread
//! running the `CFRunLoop`. Work that arrives anywhere else — an IPC request,
//! a finished script — has to be handed over rather than acted on. A manual
//! run loop source is the handover: the worker signals, the run loop wakes and
//! runs the handler on the main thread.

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFIndex, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use std::ffi::c_void;

/// A handle a worker thread can use to wake the main run loop.
///
/// Signals coalesce: several wakes before the run loop gets a turn produce one
/// call of the handler, so the handler must drain whatever queued up rather
/// than assume one item per call.
#[derive(Clone)]
pub struct Waker {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

// SAFETY: `CFRunLoopSource::signal` and `CFRunLoop::wake_up` are the two calls
// CoreFoundation documents as safe from any thread, and they are all this
// exposes. The handler itself never leaves the thread that installed it.
unsafe impl Send for Waker {}
unsafe impl Sync for Waker {}

/// The boxed handler, kept alive by CoreFoundation's retain/release of `info`.
/// Source ordering relative to other sources; nothing here competes.
const ORDER: CFIndex = 0;

struct Handler<F> {
    ref_count: isize,
    func: F,
}

impl Waker {
    /// Installs a manual source on the current thread's run loop.
    ///
    /// The handler runs in all common modes, so it still fires while a menu is
    /// tracking or a window is being resized.
    ///
    /// # Panics
    ///
    /// Panics if the current thread has no run loop, or if CoreFoundation
    /// declines to create the source — neither is recoverable.
    pub fn install<F: Fn() + 'static>(handler: F) -> Self {
        unsafe extern "C-unwind" fn perform<F: Fn() + 'static>(info: *mut c_void) {
            // SAFETY: `info` is the pointer installed below, still retained.
            let handler = unsafe { &*info.cast::<Handler<F>>() };
            (handler.func)();
        }
        unsafe extern "C-unwind" fn retain<F>(info: *const c_void) -> *const c_void {
            // SAFETY: as above; CoreFoundation calls this only on our pointer.
            let handler = unsafe { &mut *info.cast::<Handler<F>>().cast_mut() };
            handler.ref_count += 1;
            info
        }
        unsafe extern "C-unwind" fn release<F>(info: *const c_void) {
            // SAFETY: as above. At zero the box is reclaimed.
            let handler = unsafe { &mut *info.cast::<Handler<F>>().cast_mut() };
            handler.ref_count -= 1;
            if handler.ref_count == 0 {
                drop(unsafe { Box::from_raw(info.cast::<Handler<F>>().cast_mut()) });
            }
        }

        let boxed = Box::into_raw(Box::new(Handler {
            ref_count: 0,
            func: handler,
        }));
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info: boxed.cast::<c_void>(),
            retain: Some(retain::<F>),
            release: Some(release::<F>),
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(perform::<F>),
        };

        // SAFETY: `context` outlives the call, and CoreFoundation copies it.
        let source = unsafe { CFRunLoopSource::new(None, ORDER, &raw mut context) }
            .expect("failed to create a run loop source");
        let run_loop = CFRunLoop::current().expect("no run loop on this thread");
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });

        Self { source, run_loop }
    }

    /// Schedules the handler and wakes the run loop so it runs promptly.
    pub fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}

/// A repeating timer on the current thread's run loop.
///
/// Held rather than detached: dropping it invalidates the timer, so a bar that
/// goes away stops ticking instead of firing into freed state.
pub struct Timer {
    timer: CFRetained<CFRunLoopTimer>,
}

impl Timer {
    /// Installs a timer firing every `interval` seconds, starting one interval
    /// from now.
    ///
    /// # Panics
    ///
    /// Panics if the current thread has no run loop, or if CoreFoundation
    /// declines to create the timer.
    pub fn every<F: Fn() + 'static>(interval: f64, handler: F) -> Self {
        unsafe extern "C-unwind" fn fire<F: Fn() + 'static>(
            _timer: *mut CFRunLoopTimer,
            info: *mut c_void,
        ) {
            // SAFETY: `info` is the pointer installed below, still retained.
            let handler = unsafe { &*info.cast::<Handler<F>>() };
            (handler.func)();
        }
        unsafe extern "C-unwind" fn retain<F>(info: *const c_void) -> *const c_void {
            // SAFETY: as above.
            let handler = unsafe { &mut *info.cast::<Handler<F>>().cast_mut() };
            handler.ref_count += 1;
            info
        }
        unsafe extern "C-unwind" fn release<F>(info: *const c_void) {
            // SAFETY: as above. At zero the box is reclaimed.
            let handler = unsafe { &mut *info.cast::<Handler<F>>().cast_mut() };
            handler.ref_count -= 1;
            if handler.ref_count == 0 {
                drop(unsafe { Box::from_raw(info.cast::<Handler<F>>().cast_mut()) });
            }
        }

        let boxed = Box::into_raw(Box::new(Handler {
            ref_count: 0,
            func: handler,
        }));
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: boxed.cast::<c_void>(),
            retain: Some(retain::<F>),
            release: Some(release::<F>),
            copyDescription: None,
        };

        let fire_at = CFAbsoluteTimeGetCurrent() + interval;
        // SAFETY: `context` outlives the call, and CoreFoundation copies it.
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                fire_at,
                interval,
                0,
                ORDER,
                Some(fire::<F>),
                &raw mut context,
            )
        }
        .expect("failed to create a run loop timer");

        let run_loop = CFRunLoop::current().expect("no run loop on this thread");
        run_loop.add_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });
        Self { timer }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.timer.invalidate();
    }
}

/// Drains the queues the run loop does not.
///
/// Two of them, and neither is served by `CFRunLoopRunInMode`.
///
/// `AppKit` keeps its own queue; a window server window's events arrive there and
/// are only delivered once something dequeues them.
///
/// Carbon keeps a *separate* one, and this does **not** drain it — see
/// [`crate::sources::mouse`] for why that is still unfinished. Draining it
/// naively, by sending every received event to the dispatcher target, exits the
/// process: some of what arrives is a system event that means quit.
pub fn pump_platform_events() {
    pump_cocoa();
}

fn pump_cocoa() {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let distant_past = objc2_foundation::NSDate::distantPast();

    // An autorelease pool per pass: dequeuing hands back autoreleased objects,
    // and without one they accumulate for the life of the process.
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: dequeuing and dispatching on the main thread, which the
        // marker above proves we are on.
        while let Some(event) = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                objc2_app_kit::NSEventMask::Any,
                Some(&distant_past),
                objc2_foundation::NSDefaultRunLoopMode,
                true,
            )
        } {
            app.sendEvent(&event);
        }
    });
}
