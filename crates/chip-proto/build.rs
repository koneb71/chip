fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Generate both client and server stubs from the shared proto definition.
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["../../proto/chip.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/chip.proto");
    Ok(())
}
