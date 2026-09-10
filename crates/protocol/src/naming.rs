//! The product's own name, in one place.
//!
//! The name used to be spelled out at every site that needed it — the Mach
//! service, six environment variables, the config directory and its entry
//! file, the Lua module a config requires, the log filter. Renaming meant
//! finding all of them, and missing one meant a daemon that answered on one
//! name and looked for its config under another.
//!
//! So none of those are literals any more. [`name!`] is, and everything else
//! is built from it with `concat!`, which happens at compile time and so costs
//! nothing at runtime and cannot drift.

/// The product name, and the prefix its environment variables take.
///
/// **The two literals below are the only place either appears.** Renaming the
/// product is editing these two lines; there is no third spelling anywhere,
/// and `concat!` means a mismatch between them is the only mistake left to
/// make.
///
/// Two rather than one because a `const fn` cannot upper-case a string in a
/// context `concat!` accepts, and pulling in a crate that can would be a
/// dependency for two lines.
///
/// ```
/// # use coolabah_protocol::name;
/// assert_eq!(name!(), "coolabah");
/// assert_eq!(name!(env "SERVICE"), "COOLABAH_SERVICE");
/// assert_eq!(name!(rc), "coolabahrc");
/// assert_eq!(name!(service), "com.auscyber.coolabah");
/// ```
#[macro_export]
macro_rules! name {
    () => {
        "coolabah"
    };
    (env $suffix:literal) => {
        concat!("COOLABAH_", $suffix)
    };

    // Everything below is derived, and needs no editing on a rename.

    // The entry file inside the config directory.
    (rc) => {
        concat!($crate::name!(), "rc")
    };
    // The Mach service the daemon answers on by default.
    (service) => {
        concat!("com.auscyber.", $crate::name!())
    };
    // A service name for one test, unique to the process running it.
    (service $test:expr) => {
        format!(
            "{}.test.{}.{}",
            $crate::name!(service),
            $test,
            std::process::id()
        )
    };
}

/// The environment variable prefix, as a value rather than a literal.
///
/// For the one caller that needs to ask whether a *name it was given* is
/// prefixed, rather than to name a variable itself.
pub const ENV_PREFIX: &str = name!(env "");
