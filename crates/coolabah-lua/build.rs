//! macOS's `ld64` already keeps a Rust binary's linked-in `LuaJIT` C symbols
//! (`lua_pushstring` and friends) visible to a `dlopen`-ed module's
//! `-undefined dynamic_lookup` flat-namespace lookup with no extra flag —
//! verified in `require::tests::requires_a_native_module_through_package_cpath`,
//! in both debug and `lto = true` release builds. This is passed anyway,
//! belt-and-suspenders against a future strip/LTO change silently breaking
//! that for the one binary that hosts its own vendored `LuaJIT`
//! (`vendored`/`unsafe_new`) and therefore actually needs it — the `module`
//! cdylib is loaded *into* a foreign host instead, and leaves its own
//! `lua_*` symbols undefined on purpose (see `Cargo.toml`).

fn main() {
    #[cfg(all(target_os = "macos", feature = "vendored"))]
    println!("cargo:rustc-link-arg-bin=coolabah-lua=-Wl,-export_dynamic");
}
