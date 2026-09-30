fn main() {
    slint_build::compile("ui/app.slint").expect("interface Slint");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if windows {
        // Windows : bibliothèque d'import de libmpv (mpv.lib) dans le dossier MPV_LIB_DIR,
        // préparé par le workflow de build (paquet « mpv-dev » de shinchiro).
        println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
        if let Ok(dir) = std::env::var("MPV_LIB_DIR") {
            println!("cargo:rustc-link-search=native={dir}");
        }
        // Icône et métadonnées de l'exécutable.
        #[cfg(windows)]
        if std::path::Path::new("assets/icon.ico").exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico").set("ProductName", "YouTube Live Translator").set("FileDescription", "YouTube Live Translator");
            res.compile().expect("ressources Windows");
        }
    } else {
        // macOS : libmpv (brew install mpv) dans /opt/homebrew/lib (Apple Silicon) ou /usr/local/lib (Intel).
        for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
            if std::path::Path::new(dir).join("libmpv.dylib").exists() {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
    }
}
