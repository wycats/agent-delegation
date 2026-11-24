fn main() {
    // include_dir! embeds the whole build, including content-hashed chunks.
    println!("cargo:rerun-if-changed=ui/build");
    assert!(
        std::path::Path::new("ui/build/index.html").is_file(),
        "Build the settings page first: cd ui && npm ci && npm run build"
    );
}
