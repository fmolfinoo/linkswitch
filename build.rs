use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Defaults give us requestedExecutionLevel=asInvoker, dpiAwareness=PerMonitorV2,
        // longPathAware, UTF-8 active code page and Common-Controls 6.0.0.0.
        //
        // asInvoker is load-bearing: this same binary is BOTH the unelevated widget and the
        // elevated worker. A requireAdministrator manifest would UAC-prompt on every plain
        // launch and destroy the whole design. The worker gets its elevated token from the
        // scheduled task instead.
        embed_manifest(new_manifest("dev.linkswitch.LinkSwitch")).expect("embed manifest");

        // Icon-only .rc. It must not contain a manifest directive or it fights embed-manifest.
        if std::path::Path::new("assets/app.ico").exists() {
            embed_resource::compile("assets/app.rc", embed_resource::NONE)
                .manifest_optional()
                .expect("embed icon");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
