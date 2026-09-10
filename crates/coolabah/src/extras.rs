//! Which running application really owns a menu bar status item.
//!
//! macOS 26 draws most extras through Control Centre, so the window server
//! reports `Control Centre` as the owner of windows belonging to half a dozen
//! other applications. The only place the truth survives is each
//! application's own `AXExtrasMenuBar`, so this walks those and matches them
//! back to windows by title and position -- the same two-pass tolerance
//! `SketchyBar`'s `source_pid_for_window_with_name_hint` uses.
//!
//! Policy only. The Accessibility calls themselves, and the permission they
//! need, are [`skylight::ax`].

use crate::alias::{point_distance, rect_center};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFArray, CFRetained, CGRect, Type};
use skylight::Result;
use skylight::ax::{self, Trusted};
use skylight::main_thread;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long one application may take to answer a status-item question before
/// the walk gives up on it.
///
/// The Accessibility API's own default is around six seconds *per element*,
/// so a single application that has stopped answering costs the whole walk
/// six seconds — the difference between a scan and a stall. Nothing here
/// needs an application that slow: a status item is drawn at frame rate, and
/// an application that cannot describe one within a quarter of a second is
/// reported as having none, which is a state every caller already handles.
pub const AX_TIMEOUT: Duration = Duration::from_millis(250);

/// One entry from a running application's `AXExtrasMenuBar`: a status
/// item this process can plausibly be the real owner of.
pub(crate) struct ExtrasMenuItem {
    pub owner: String,
    pub title: String,
    pub pid: i32,
    pub frame: CGRect,
}

/// Every running application, as a plain `(pid, name)` list.
///
/// `NSWorkspace` is `AppKit`, and `AppKit`'s thread-safety off the main
/// thread is not something Apple guarantees, so this — the one `AppKit` call
/// the walk needs — stays here, witnessed by the marker. It does no Accessibility work
/// and costs microseconds; only what it returns crosses to a worker.
#[main_thread]
pub(crate) fn running_apps() -> Vec<(i32, String)> {
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .runningApplications()
        .to_vec()
        .iter()
        .filter(|app| app.processIdentifier() > 0)
        .map(|app| {
            let pid = app.processIdentifier();
            let owner = app
                .localizedName()
                .map_or_else(|| pid.to_string(), |name| name.to_string());
            (pid, owner)
        })
        .collect()
}

/// One application's own status items.
///
/// Split out of the walk so it can run anywhere: `AXUIElement` is not
/// main-thread-only and each call is an independent Mach round trip, so a
/// pool of workers can ask a hundred applications at once instead of one
/// after another. Nothing CoreFoundation-shaped comes back — the result is
/// plain strings, an integer and a rectangle — which is what makes crossing
/// the thread boundary cheap and `Send` honest.
///
/// The grant is *not* re-checked here: `AXIsProcessTrusted` reads the TCC
/// database, and asking it once per application is a real cost for an answer
/// that cannot change mid-walk. [`enumerate_extras_menu_items`] and
/// [`Scanner::request`] check it once, up front.
#[must_use]
pub(crate) fn extras_for_pid(
    access: Trusted,
    pid: i32,
    owner: &str,
    timeout: Option<Duration>,
) -> Vec<ExtrasMenuItem> {
    let children = match timeout {
        Some(timeout) => ax::extras_menu_children_within(access, pid, timeout),
        None => ax::extras_menu_children(access, pid),
    };
    let Some(children) = children else {
        return Vec::new();
    };
    let mut items = Vec::new();
    collect_items(&children, pid, owner, &mut items);
    items
}

/// Walks every running application's extras menu bar, serially, on the
/// calling thread.
///
/// The blocking form, for a one-shot caller with nowhere to wait — the CLI's
/// `--query default_menu_items`, a `press`, the probe. The daemon uses
/// [`Scanner`] instead, which does the same walk concurrently and off the
/// main thread.
///
/// # Errors
///
/// [`skylight::Error::NotTrusted`] without the Accessibility grant. Every
/// answer here is empty without it, which is exactly what a machine with no
/// status items at all looks like, so it is reported rather than guessed at.
#[main_thread(pass)]
pub(crate) fn enumerate_extras_menu_items() -> Result<Vec<ExtrasMenuItem>> {
    let access = Trusted::get()?;
    Ok(running_apps(mtm)
        .into_iter()
        .flat_map(|(pid, owner)| extras_for_pid(access, pid, &owner, Some(AX_TIMEOUT)))
        .collect())
}

/// Turns one application's `AXExtrasMenuBar` children into candidates.
fn collect_items(children: &CFArray, pid: i32, owner: &str, items: &mut Vec<ExtrasMenuItem>) {
    for i in 0..children.count() {
        // SAFETY: `i` is in bounds; every element of an
        // `AXUIElement`'s children attribute is itself an
        // `AXUIElement`.
        let Some(child) = ax::array_element(children, i) else {
            continue;
        };

        if !ax::attribute::<bool>(child, "AXEnabled").unwrap_or(true) {
            continue;
        }
        // Not an `AXRole == "AXButton"` filter here, deliberately,
        // even though `alias_watch` uses exactly that to clear
        // notification noise: measured live, several real,
        // correctly-attributed items on this machine — OneDrive's,
        // Spotlight's, Fantastical's own status items among them —
        // do not report that role from their own `AXExtrasMenuBar`
        // child, so requiring it here silently dropped them from the
        // candidate list and collapsed their recovered owner back to
        // plain "Control Center" for everything. `alias_watch`'s use
        // is narrower and safe: it only ever discards notifications
        // already known to share a *watched* pid, not a whole
        // app's candidacy for owner recovery.
        //
        // Control Centre's own children have no `AXTitle` at all —
        // their identifying string lives in `AXDescription` instead
        // (confirmed live by `examples/ax_observer_probe.rs`, e.g.
        // `"Clock"`). `AXDescription` is checked first to match
        // `alias_watch`'s own precedence.
        let description = ax::attribute::<String>(child, "AXDescription").filter(|d| !d.is_empty());
        let title = ax::attribute::<String>(child, "AXTitle").filter(|t| !t.is_empty());
        let Some(identity) = description.or(title) else {
            continue;
        };
        let Some(frame) = ax::frame(child) else {
            continue;
        };

        items.push(ExtrasMenuItem {
            owner: owner.to_owned(),
            title: identity,
            pid,
            frame,
        });
    }
}

/// Matches a captured window's name and bounds against the extras
/// gathered by [`enumerate_extras_menu_items`], the same two-pass
/// tolerance `SketchyBar`'s `source_pid_for_window_with_name_hint` uses:
/// first an exact title match within a tight radius, then bounds alone
/// within a looser one for items whose title didn't round-trip.
pub(crate) fn resolve<'a>(
    window_name: &str,
    bounds: CGRect,
    extras: &'a [ExtrasMenuItem],
) -> Option<&'a ExtrasMenuItem> {
    const TIGHT_RADIUS: f64 = 6.0;
    const TIGHT_SIZE_TOLERANCE: f64 = 8.0;
    const LOOSE_RADIUS: f64 = 14.0;

    let center = rect_center(bounds);
    let close_enough = |item: &&ExtrasMenuItem| {
        let d = point_distance(center, rect_center(item.frame));
        let w_diff = (item.frame.size.width - bounds.size.width).abs();
        let h_diff = (item.frame.size.height - bounds.size.height).abs();
        d <= TIGHT_RADIUS && w_diff <= TIGHT_SIZE_TOLERANCE && h_diff <= TIGHT_SIZE_TOLERANCE
    };

    if !window_name.is_empty() {
        let named = extras
            .iter()
            .filter(|item| item.title == window_name)
            .filter(close_enough)
            .min_by(|a, b| {
                point_distance(center, rect_center(a.frame))
                    .total_cmp(&point_distance(center, rect_center(b.frame)))
            });
        if named.is_some() {
            return named;
        }
    }

    extras
        .iter()
        .filter(|item| point_distance(center, rect_center(item.frame)) <= LOOSE_RADIUS)
        .min_by(|a, b| {
            point_distance(center, rect_center(a.frame))
                .total_cmp(&point_distance(center, rect_center(b.frame)))
        })
}

/// How far a candidate `AXExtrasMenuBar` child's on-screen frame may sit
/// from the resolved window's own bounds and still be trusted as the
/// element behind it, for [`find_extras_child`]'s fallback. Looser than
/// [`resolve`]'s own `LOOSE_RADIUS` (14pt): that constant tolerates a
/// window-vs-AX-frame mismatch while still searching *every* running
/// application for the right owner; here the owner is already known and
/// only its own items are candidates, so a same-pid neighbour a little
/// further off is still far more likely to be a mismeasurement than a
/// different item.
const PRESS_MATCH_RADIUS: f64 = 20.0;

/// How many levels of `AXChildren`/`AXVisibleChildren` [`find_extras_child`]
/// descends into looking for a pressable icon. Control Centre groups
/// several of its own modules (Wi-Fi, Bluetooth, `AirDrop`, ...) into one
/// compact "`BentoBox`" element that draws as several separate windows at
/// the window-server level — which is what [`crate::alias::disambiguate_duplicates`]
/// numbers as distinct `Item-0(n)`s — but exposes them as *grandchildren*
/// of `AXExtrasMenuBar`, one level below its own `AXVisibleChildren`.
/// Measured live: pressing a `BentoBox`-hosted item with a flat,
/// one-level walk found no candidate anywhere near its window (the
/// nearest real, top-level child sat 189pt away, well outside
/// [`PRESS_MATCH_RADIUS`]), while its actual `AXButton` element was one
/// level down inside the box. One level is enough for every case seen so
/// far; nothing here rules out a deeper nesting existing.
const MAX_DEPTH: u32 = 1;

/// Collects every candidate in `children`, recursing into any child that
/// is itself a container (own `AXChildren`/`AXVisibleChildren`) up to
/// [`MAX_DEPTH`] further levels — see its doc comment for why a flat walk
/// misses `BentoBox`-hosted icons.
fn collect_candidates(
    children: &CFArray,
    depth: u32,
    out: &mut Vec<(CFRetained<AXUIElement>, Option<String>, Option<CGRect>)>,
) {
    for i in 0..children.count() {
        // SAFETY: `i` is in bounds; every element is an `AXUIElement`,
        // per `array_element`'s own contract.
        let Some(child) = ax::array_element(children, i) else {
            continue;
        };
        let identity = ax::attribute::<String>(child, "AXDescription")
            .filter(|d| !d.is_empty())
            .or_else(|| ax::attribute::<String>(child, "AXTitle").filter(|t| !t.is_empty()));
        let frame = ax::frame(child);
        out.push((child.retain(), identity, frame));

        if depth > 0
            && let Some(grandchildren) =
                ax::attribute::<CFRetained<CFArray>>(child, "AXVisibleChildren")
                    .or_else(|| ax::attribute::<CFRetained<CFArray>>(child, "AXChildren"))
        {
            collect_candidates(&grandchildren, depth - 1, out);
        }
    }
}

/// Finds the `AXExtrasMenuBar` child behind an already-resolved window,
/// for [`crate::alias::press_item`]. Unlike [`resolve`], the pid is already
/// known, so only that one application's extras are walked rather than
/// every running one.
///
/// Prefers an exact `AXDescription`/`AXTitle` match (same precedence as
/// [`enumerate_extras_menu_items`]), then falls back to the nearest
/// candidate by frame within [`PRESS_MATCH_RADIUS`] — what a disambiguated
/// `(n)` name always needs, since the real identity string never carries
/// that suffix. Candidates come from [`collect_candidates`], so a
/// `BentoBox`-nested icon is found the same way a top-level one is.
///
/// # Errors
///
/// [`skylight::Error::NotTrusted`] without the Accessibility grant. An
/// application that publishes no extras, or none near enough to be this
/// window, is `Ok(None)`.
pub(crate) fn find_extras_child(
    pid: i32,
    name_hint: &str,
    frame_hint: CGRect,
) -> Result<Option<CFRetained<AXUIElement>>> {
    let access = Trusted::get()?;
    let Some(children) = ax::extras_menu_children_within(access, pid, AX_TIMEOUT) else {
        return Ok(None);
    };

    let mut candidates = Vec::new();
    collect_candidates(&children, MAX_DEPTH, &mut candidates);

    let mut nearest: Option<(CFRetained<AXUIElement>, f64)> = None;
    for (child, identity, frame) in candidates {
        if identity.as_deref() == Some(name_hint) {
            return Ok(Some(child));
        }
        let Some(frame) = frame else {
            continue;
        };
        let distance = point_distance(rect_center(frame), rect_center(frame_hint));
        if nearest.as_ref().is_none_or(|(_, best)| distance < *best) {
            nearest = Some((child, distance));
        }
    }

    let Some((element, distance)) = nearest else {
        return Ok(None);
    };
    Ok((distance <= PRESS_MATCH_RADIUS).then_some(element))
}

/// The Accessibility walk, run off the main thread and fanned out across it.
///
/// Two costs were conflated before this existed. Listing the menu bar layer
/// is a window-server call: fast, and legal only on the main thread. Walking
/// every running application's `AXExtrasMenuBar` is neither — it is dozens of
/// synchronous Mach round trips into other processes, measured on this
/// machine at over a second — and it is the only slow half. This owns the
/// slow half.
///
/// **A worker blocked in `AXUIElementCopyAttributeValue` is doing work.** It
/// is the same category as `script.rs`'s four subprocess workers, not a
/// thread parked on I/O a reactor should have owned: there is no
/// file descriptor to wait on and no readiness to subscribe to, only a
/// synchronous RPC that another process answers when it feels like it. Read
/// literally, "no blocked threads" would forbid ever asking another process a
/// question, which is not what it means. What it does mean is that the thread
/// that composites the bar is never the one waiting.
///
/// Fanned out rather than merely moved, because the walk's cost is the *sum*
/// of every application's latency when it is serial. One task per pid makes
/// it the slowest single application instead — and [`AX_TIMEOUT`] caps even
/// that. Measured on this machine: 2.6 s serial under the API's own default
/// timeout, of which one unresponsive application was 1.5 s; 277 ms serial
/// with the timeout; the slowest single application with it, which is what
/// this costs, 255 ms — and none of it on the main thread.
///
/// The threads are [`crate::pool`]'s, not this type's. What is here instead is
/// [`Scanner::permits`], because the *reason* to bound this work is a fact
/// about this walk — a login session's applications, each answering at its own
/// pace — and not about the machine. A scan already in flight outlives the
/// [`Scanner`] that asked for it: it finishes into `Arc`s the tasks hold
/// themselves, so nothing writes into freed memory, and the wake it sends
/// lands on a run loop source that outlives the process either way.
pub(crate) struct Scanner {
    /// How many applications may be asked at once; see [`SCAN_WORKERS`].
    permits: Arc<tokio::sync::Semaphore>,
    wake: crate::runloop::Waker,
    /// Set while a scan is out. A config reload touching twenty-nine aliases
    /// asks twenty-nine times and gets one scan.
    running: Arc<AtomicBool>,
    /// Where a finished scan lands, until the main thread collects it.
    finished: Arc<Mutex<Option<Vec<ExtrasMenuItem>>>>,
    /// Whether [`Scanner::finished`] holds anything, readable without the
    /// lock. The run condition asks on every pass and must not pay for it --
    /// and paying here is worse than a wasted instruction, because the lock
    /// it would take is one a worker holds while publishing a scan. The same
    /// reason [`crate::alias_watch`]'s dirty flag is an atomic beside its set.
    landed: Arc<AtomicBool>,
    /// How many scans have completed.
    ///
    /// What lets a caller tell "no scan has answered since I last asked" from
    /// "one has, and it did not help". Without it a burst of new aliases —
    /// a config adding seventy-five in one go — starts a fresh walk of every
    /// running application each time one lands, because `request` only
    /// suppresses an ask while a scan is *running*: measured at 38 walks of
    /// 123 applications in the 250 ms a stress config takes to load.
    generation: Arc<AtomicU64>,
}

thread_local! {
    /// The most recent completed scan.
    ///
    /// A fact about the machine rather than about whoever started the scan:
    /// [`Scanner`] runs it, but a click opening a mirrored menu and a
    /// `--query default_menu_items` both want the same answer and neither
    /// should start a walk of its own to get it. The daemon runs its whole
    /// world on one thread, so one cell is the whole process — and a thread
    /// without a run loop simply sees `None`, which is the same answer as
    /// "no scan has landed yet".
    static LATEST: RefCell<Option<Vec<ExtrasMenuItem>>> = const { RefCell::new(None) };
}

/// Reads the last completed scan in place, without cloning it.
pub(crate) fn with_last_scan<R>(f: impl FnOnce(Option<&[ExtrasMenuItem]>) -> R) -> R {
    LATEST.with_borrow(|held| f(held.as_deref()))
}

/// Whether any scan has landed yet.
pub(crate) fn scanned() -> bool {
    LATEST.with_borrow(Option::is_some)
}

/// Stores a scan a caller did itself, so everything after it reads the same
/// answer rather than walking again.
pub(crate) fn remember_scan(items: Vec<ExtrasMenuItem>) {
    LATEST.set(Some(items));
}

/// How many applications the scan asks at once.
///
/// Not the CPU count: each of these is parked in another process's reply, not
/// computing, so sizing for cores would leave the machine idle waiting on a
/// queue. Sized instead so a typical login session's applications are covered
/// in a couple of rounds, each round bounded by [`AX_TIMEOUT`].
///
/// It is a bound and not a thread count. `AXUIElementCopyAttributeValue` is a
/// synchronous cross-process Mach RPC with no async form, so each of these does
/// occupy a thread while it waits — but one of
/// [`pool::blocking`](crate::pool::blocking)'s, which exist only while
/// something is actually blocked in them, rather than sixteen held for the life
/// of the process against a walk that happens when an application launches.
const SCAN_WORKERS: usize = 16;

impl Scanner {
    /// Remembers how to wake the run loop when a scan lands.
    pub(crate) fn new(wake: crate::runloop::Waker) -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(SCAN_WORKERS)),
            wake,
            running: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(Mutex::new(None)),
            landed: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Starts a scan, unless one is already out. Answers whether this call
    /// was the one that started it -- `false` means a scan was already
    /// running and nothing new was asked for.
    ///
    /// Idempotent on purpose: every caller here asks because something
    /// happened that *might* have changed who owns what, and several such
    /// things happen at once — a config run adds every alias in one pass.
    /// The answer one scan produces serves all of them.
    ///
    /// The return value is what lets [`crate::alias::Captures::ask_for_scan`]
    /// tell "I asked and a scan is now out" from "I asked and nothing
    /// happened" -- the two used to look the same from outside, which let it
    /// record having asked for a generation no scan would ever complete.
    ///
    /// Takes the grant as a value rather than checking for one, so there is
    /// no untrusted path through here to report on: what to do about a
    /// missing grant -- prompt once, say so once -- is [`crate::alias`]'s
    /// decision and is made in one place.
    #[main_thread(pass)]
    pub(crate) fn request(&self, access: Trusted) -> bool {
        if self.running.swap(true, Ordering::AcqRel) {
            return false;
        }

        // The one AppKit call, made here rather than on a worker: what
        // crosses is a plain `Vec<(i32, String)>`.
        let apps = running_apps(mtm);
        tracing::debug!(apps = apps.len(), "scanning for menu bar item owners");
        let tasks: Vec<_> = apps
            .into_iter()
            .map(|(pid, owner)| {
                let permits = Arc::clone(&self.permits);
                crate::pool::spawn(async move {
                    // The wait for a turn is the async half; the ask itself is
                    // the part with no async form. Holding the permit across
                    // both is what makes this a bound on applications being
                    // asked rather than on tasks existing.
                    let _turn = permits.acquire_owned().await;
                    crate::pool::blocking(move || {
                        extras_for_pid(access, pid, &owner, Some(AX_TIMEOUT))
                    })
                    .await
                    .unwrap_or_default()
                })
            })
            .collect();

        let finished = Arc::clone(&self.finished);
        let landed = Arc::clone(&self.landed);
        let running = Arc::clone(&self.running);
        let generation = Arc::clone(&self.generation);
        let wake = self.wake.clone();
        crate::pool::spawn(async move {
            let began = std::time::Instant::now();
            let total = tasks.len();
            let mut done = 0usize;
            let mut items = Vec::new();
            for task in tasks {
                // A task that panicked contributes nothing rather than
                // taking the scan down with it: one application answering
                // badly is not a reason to lose the other hundred.
                items.extend(task.await.unwrap_or_default());
                done += 1;
            }
            tracing::debug!(
                total,
                done,
                ?began,
                elapsed = ?began.elapsed(),
                found = items.len(),
                "every per-pid scan task joined"
            );
            *finished.lock().unwrap_or_else(PoisonError::into_inner) = Some(items);
            landed.store(true, Ordering::Release);
            // Both published before the wake, so the pass this wakes sees
            // a finished scan and a moved generation together.
            generation.fetch_add(1, Ordering::AcqRel);
            running.store(false, Ordering::Release);
            wake.wake();
        });
        true
    }

    /// Collects a finished scan, if one is waiting, into [`with_last_scan`].
    /// `true` when this call replaced what that answers.
    ///
    /// Cleared unconditionally, under the same lock as the take -- not only
    /// when something was actually taken. A scan publishes as two separate
    /// steps, `*finished.lock() = Some(_)` and then `landed.store(true)` (see
    /// [`Scanner::request`]); a `take` landing in the gap between them would
    /// drain the value while `landed` was still false, and clearing only
    /// when something was found would never see the flag again once the
    /// worker's own store lands against an already-empty slot -- `landed`
    /// would read true forever after. Clearing every call self-heals from
    /// that race, the same trade [`crate::alias::Captor::take`] makes.
    pub(crate) fn take(&self) -> bool {
        let mut held = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        let items = held.take();
        self.landed.store(false, Ordering::Release);
        drop(held);
        let Some(items) = items else {
            return false;
        };
        tracing::debug!(items = items.len(), "a menu bar owner scan came back");
        LATEST.set(Some(items));
        true
    }

    /// Whether a finished scan is waiting to be collected — the run
    /// condition's half of [`Scanner::take`].
    pub(crate) fn ready(&self) -> bool {
        self.landed.load(Ordering::Acquire)
    }
}
