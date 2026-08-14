fn main() {
    // The shared auth host is baked in via option_env!; without this, changing
    // it would not invalidate the cached build and the old host would ship.
    println!("cargo:rerun-if-env-changed=OPENFRAME_SHARED_HOST_URL");
    println!("cargo:rerun-if-env-changed=OPENFRAME_UPDATE_MANIFEST_URL");
    tauri_build::build()
}
