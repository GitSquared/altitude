use std::{env, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=resources/macos/Info.plist");
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|target| target == "macos") {
        let manifest =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
        let plist = manifest.join("resources/macos/Info.plist");
        println!(
            "cargo::rustc-link-arg-bin=altitude=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
