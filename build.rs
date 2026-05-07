use std::path::Path;

fn main() {
    let system_libxdo = Path::new("/run/current-system/sw/lib/libxdo.so");
    if system_libxdo.exists() {
        println!("cargo:rustc-link-search=native=/run/current-system/sw/lib");
    }
}
