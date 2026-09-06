use std::fs;
use std::path::Path;

const DEFAULT_SDK: &str = "MacOSX.sdk";

// The window server frameworks live in PrivateFrameworks, which is not on the
// default search path. Resolve it relative to the active SDK rather than the
// running system, so a nix/devenv build links against the SDK it was given.
fn main() {
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");

    let sdk_dir = std::env::var("DEVELOPER_DIR").map_or_else(
        |_| "/Library/Developer/CommandLineTools/SDKs".to_string(),
        |dir| format!("{dir}/Platforms/MacOSX.platform/Developer/SDKs"),
    );

    let bases = std::iter::once(format!("{sdk_dir}/{DEFAULT_SDK}")).chain(
        fs::read_dir(&sdk_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(String::from))
            // The default filesystem is case-insensitive, so the on-disk
            // spelling of the extension is not guaranteed to be lowercase.
            .filter(|name| {
                name.starts_with("MacOSX")
                    && name != DEFAULT_SDK
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("sdk"))
            })
            .map(|name| format!("{sdk_dir}/{name}")),
    );

    for base in bases {
        let private = format!("{base}/System/Library/PrivateFrameworks");
        if Path::new(&private).exists() {
            println!("cargo:rustc-link-search=framework={private}");
        }
    }

    println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");
    println!("cargo:rustc-link-lib=framework=SkyLight");
}
