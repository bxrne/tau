#![no_main]
use libfuzzer_sys::fuzz_target;
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
        // parse must never panic on arbitrary input
        let _ = libtau::parse(s);
    }
});
