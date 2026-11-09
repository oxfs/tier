fn main() {
    println!("cargo:rerun-if-changed=proto/tier.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("bundled protoc");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(&["proto/tier.proto"], &["proto"])
        .expect("compile Tier IPC schema");
}
