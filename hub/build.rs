// The web app is embedded with `include_dir!`, which cannot tell cargo
// about files added to or removed from `web/`: this does. BUNDLE_ID is
// read at compile time (`option_env!`), so a change must rebuild.
fn main() {
    println!("cargo:rerun-if-changed=../web");
    println!("cargo:rerun-if-changed=build.rs");
    // Baked into the service label (service.rs `label`).
    println!("cargo:rerun-if-env-changed=BUNDLE_ID");
}
