fn main() {
    slint_build::compile("ui/app.slint").expect("interface Slint");
    // libmpv (brew install mpv) : Homebrew l'installe dans /opt/homebrew/lib (Apple Silicon)
    // ou /usr/local/lib (Intel).
    for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
        if std::path::Path::new(dir).join("libmpv.dylib").exists() {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
}
