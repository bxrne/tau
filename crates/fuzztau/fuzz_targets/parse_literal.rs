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
        // parse_literal is a public helper for bulk-load paths.
        // It must never panic and should only return Some on a single clean literal.
        let _ = libtau::parse_literal(s);
    }
});
