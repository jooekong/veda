//! Generates the OTLP metrics gRPC client from the vendored legacy proto
//! (see `proto/PROVENANCE.md`). Client-only — veda pushes metrics, never serves.

fn main() {
    // Build hosts (.161 dogfood, CI) have no system protoc; point prost at the
    // vendored binary so no manual install is needed on any build host.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(false)
        .compile_protos(
            &["proto/opentelemetry/proto/collector/metrics/v1/metrics_service.proto"],
            &["proto"],
        )
        .expect("compile vendored OTLP metrics proto");

    println!("cargo:rerun-if-changed=proto");
}
