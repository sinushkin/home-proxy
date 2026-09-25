fn main() {
    prost_build::compile_protos(&["proto/connection.proto"], &["proto/"])
        .expect("failed to compile protobuf schema");
}
