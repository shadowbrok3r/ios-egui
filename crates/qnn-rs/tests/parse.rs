//! Env-gated integration test: parses a real QNN context binary with a real
//! libQnnSystem.so. Ignored by default (needs the proprietary SDK + a model).
//!
//! Run with:
//!   QNN_SYSTEM_LIB=.../lib/x86_64-linux-clang/libQnnSystem.so \
//!   QNN_CONTEXT_BIN=.../unet.bin \
//!   cargo test -p qnn-rs -- --ignored

use qnn_rs::{ContextBinaryInfo, QnnSystem};

#[test]
#[ignore = "requires QNN_SYSTEM_LIB and QNN_CONTEXT_BIN pointing at the SDK + a model"]
fn parse_real_context_binary() {
    let lib = std::env::var("QNN_SYSTEM_LIB").expect("set QNN_SYSTEM_LIB");
    let bin = std::env::var("QNN_CONTEXT_BIN").expect("set QNN_CONTEXT_BIN");

    let system = QnnSystem::load(&lib).expect("load libQnnSystem.so");
    let bytes = std::fs::read(&bin).expect("read context binary");
    let info = ContextBinaryInfo::parse(&system, &bytes).expect("parse metadata");

    assert!(!info.graphs.is_empty(), "expected at least one graph");
    for g in &info.graphs {
        assert!(!g.name.is_empty(), "graph name must not be empty");
        assert!(
            !g.inputs.is_empty() || !g.outputs.is_empty(),
            "graph {} has no tensors",
            g.name
        );
        for t in g.inputs.iter().chain(g.outputs.iter()) {
            assert!(!t.name.is_empty(), "tensor name must not be empty");
        }
    }
}
