// SPDX-License-Identifier: MIT
// Generates the Rust bindings for the control-plane data model (prost).
// protoc comes from protoc-bin-vendored, so no system protobuf compiler is needed.
// The .proto files under proto/ describe the wallpaper-entry / filter data model.
// They are vendored from waywallen (MIT, (c) hypengw); see proto/NOTICE.

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc missing");
    // edition 2024: set_var is unsafe
    unsafe { std::env::set_var("PROTOC", protoc) };

    let dir = std::path::Path::new("proto");
    let mut cfg = prost_build::Config::new();
    // Keep the field names from the proto (snake_case) so it stays comparable to the schema.
    cfg.compile_protos(&[dir.join("control.proto")], &[dir])
        .expect("prost-build failed to compile control.proto");

    println!("cargo:rerun-if-changed={}", dir.join("control.proto").display());
    println!("cargo:rerun-if-changed={}", dir.join("filter.proto").display());
}
