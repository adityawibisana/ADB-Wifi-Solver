fn main() {
    // The ui/ folder is embedded into the exe, so rebuild when it changes.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=capabilities");
    // Embeds the app icon and Windows manifest, and checks tauri.conf.json.
    tauri_build::build()
}
