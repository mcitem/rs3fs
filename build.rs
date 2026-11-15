fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/s3fs.rc");
        println!("cargo:rerun-if-changed=assets/s3fs.ico");
        println!("cargo:rerun-if-changed=Cargo.toml");

        let out_dir =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("missing OUT_DIR"));
        let version_header = format!(
            "#define S3FS_VERSION_NUMBERS {},{},{},0\n#define S3FS_VERSION_STRING \"{}\"\n",
            env!("CARGO_PKG_VERSION_MAJOR"),
            env!("CARGO_PKG_VERSION_MINOR"),
            env!("CARGO_PKG_VERSION_PATCH"),
            env!("CARGO_PKG_VERSION"),
        );
        std::fs::write(out_dir.join("s3fs-version.h"), version_header)
            .expect("failed to write the rs3fs version resource header");

        embed_resource::compile_for(
            "assets/s3fs.rc",
            [env!("CARGO_PKG_NAME")],
            embed_resource::ParamsIncludeDirs([out_dir.as_os_str()]),
        )
        .manifest_required()
        .expect("failed to embed the rs3fs application resources");
    }
}
