fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc_path = protoc_bin_vendored::protoc_bin_path().expect("protoc binary not found");
    std::env::set_var("PROTOC", protoc_path);
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["proto/wallet.proto", "proto/base_node.proto"], &["proto/"])?;
    Ok(())
}
