fn main() {
    // Embed the app icon into adbqr.exe on Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/adbqr.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/adbqr.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
