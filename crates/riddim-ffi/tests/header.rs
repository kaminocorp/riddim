//! The checked-in C header must be exactly what cbindgen generates from the Rust source.

use std::path::Path;

#[test]
fn header_matches_the_source() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config =
        cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).expect("cbindgen.toml");
    let mut generated = Vec::new();
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen")
        .write(&mut generated);
    let generated = String::from_utf8(generated).unwrap();

    let path = crate_dir.join("include/riddim.h");
    if std::env::var_os("RIDDIM_UPDATE_HEADER").is_some() {
        std::fs::write(&path, &generated).unwrap();
        eprintln!("updated {}", path.display());
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "include/riddim.h is out of date; regenerate it with\n  \
         RIDDIM_UPDATE_HEADER=1 cargo test -p riddim-ffi --test header"
    );
}
