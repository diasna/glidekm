fn main() {
    // Windows: embed the app icon and the manifest (common controls v6,
    // per-monitor DPI awareness, UTF-8).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=res/glidekm.rc");
        println!("cargo:rerun-if-changed=res/glidekm.manifest");
        println!("cargo:rerun-if-changed=res/glidekm.ico");
        embed_resource::compile("res/glidekm.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
