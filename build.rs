use cfg_aliases::cfg_aliases;

fn main() {
    // The script doesn't depend on our code
    println!("cargo:rerun-if-changed=build.rs");
    // But it *does* depend on cfgs!
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=RUSTDOCFLAGS");

    // Higher level config groups. This way, these short phrases can represent not only that the feature is requested
    // but also available at compile time or documenting. (ie, enabling "wayland-tablet-unstable-v2" shouldn't compile err on Windows.)
    cfg_aliases! {
        // Wayland tablet is requested and available. Adapted from winit.
        // lonngg cfg = The feature is on, and (docs or (supported platform and not unsupported platform))
        wl_tablet: { all(feature = "wayland-tablet-unstable-v2", any(docsrs, all(unix, not(any(target_os = "redox", target_family = "wasm", target_os = "android", target_os = "ios", target_os = "macos"))))) },
        // Same as above but for xlib `xinput2` support.
        xinput2: { all(feature = "xorg-xinput2", any(docsrs, all(unix, not(any(target_os = "redox", target_family = "wasm", target_os = "android", target_os = "ios", target_os = "macos"))))) },
        // Ink RealTimeStylus is requested and available
        ink_rts: { all(feature = "windows-ink", any(docsrs, target_os = "windows")) },
    }
}
