//! Embed native application identity in macOS and Windows executables.
fn main() {
    println!("cargo:rerun-if-changed=assets/Info.plist");
    println!("cargo:rerun-if-changed=assets/Nutty.ico");
    // macOS bundle versions must be plain numbers, so Info.plist carries the numeric
    // part of a pre-release such as 1.0.0-beta.1; the full version stays in `--version`.
    let part = |name: &str| std::env::var(name).expect("Cargo supplies the package version");
    let version = format!(
        "{}.{}.{}",
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH")
    );
    let plist = std::fs::read_to_string("assets/Info.plist").expect("read application plist");
    assert!(
        plist.contains(&format!(
            "<key>CFBundleShortVersionString</key><string>{version}</string>"
        )),
        "Info.plist and Cargo package versions must match"
    );
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let root =
            std::env::var("CARGO_MANIFEST_DIR").expect("Cargo supplies the manifest directory");
        println!(
            "cargo:rustc-link-arg-bin=Nutty=-Wl,-sectcreate,__TEXT,__info_plist,{root}/assets/Info.plist"
        );
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI supplies the PerMonitorV2/asInvoker manifest; this resource owns the
        // icon (ID 1, also used by GPUI) and Explorer/Task Manager version metadata.
        winresource::WindowsResource::new()
            .set_icon_with_id("assets/Nutty.ico", "1")
            .set("ProductName", "Nutty")
            .set("FileDescription", "Nutty")
            .set("OriginalFilename", "Nutty.exe")
            .set("InternalName", "Nutty")
            .set(
                "LegalCopyright",
                "Copyright © 2026 madebynut. Apache License 2.0.",
            )
            .compile()
            .expect("compile Windows application resources");
    }
}
