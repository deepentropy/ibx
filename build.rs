fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // A test binary under the `python` feature links libpython, because
    // `extension-module` — which tells PyO3 not to link it — is only enabled
    // for the wheel. Linking is not enough on its own: the loader has to find
    // the library at run time too, and without this every such binary dies with
    // "libpython…: cannot open shared object file" (ibx#381).
    #[cfg(all(feature = "python", not(feature = "extension-module")))]
    if let Some(dir) = python_lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}

#[cfg(all(feature = "python", not(feature = "extension-module")))]
fn python_lib_dir() -> Option<String> {
    let out = std::process::Command::new(
        std::env::var("PYO3_PYTHON").unwrap_or_else(|_| "python3".into()),
    )
    .args(["-c", "import sysconfig; print(sysconfig.get_config_var('LIBDIR') or '')"])
    .output()
    .ok()?;
    let dir = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if dir.is_empty() { None } else { Some(dir) }
}
