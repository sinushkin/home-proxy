fn main() {
    prost_build::compile_protos(&["proto/control.proto"], &["proto/"]).expect("failed to compile protobuf schema");
}
