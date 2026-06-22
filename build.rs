fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_airspyhf)");
    if pkg_config::Config::new().probe("libairspyhf").is_ok() {
        println!("cargo:rustc-cfg=has_airspyhf");
    } else {
        println!(
            "cargo:warning=libairspyhf not found — Airspy HF+ will be detected but cannot be opened. \
             macOS: brew install airspyhf  |  Linux: apt install libairspyhf-dev"
        );
    }
}
