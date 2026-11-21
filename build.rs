// Embeds the app icon in the .exe. GPUI loads the window/taskbar icon from
// icon resource ID 1.
fn main() {
    println!("cargo:rerun-if-changed=assets/logo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/logo.ico", "1");
        res.set("ProductName", "Epic Copy");
        res.set("FileDescription", "Epic Copy");
        res.compile().expect("failed to embed the app icon");
    }
}
