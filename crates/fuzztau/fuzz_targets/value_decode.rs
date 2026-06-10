#![no_main]
use libfuzzer_sys::fuzz_target;
use libtau::storage::Codec;
use std::sync::OnceLock;

static TRACING: OnceLock<()> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    TRACING.get_or_init(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_target(false)
            .init();
    });

    if let Ok(s) = std::str::from_utf8(data) {
        // Value::decode must never panic on arbitrary (UTF-8) input.
        // This is the decoding used by the wire protocol (VAL/RANGE segments).
        let _ = libtau::Value::decode(s);
    }
});
