//! Links the icon into the exe once build.sh has compiled it (app.rc -> target/app.res).
fn main() {
    let res = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("target").join("app.res");
    println!("cargo:rerun-if-changed={}", res.display());
    if res.exists() {
        println!("cargo:rustc-link-arg-bin=LiveSubs={}", res.display());
    }
}
