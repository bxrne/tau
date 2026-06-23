#![no_main]
use libfuzzer_sys::fuzz_target;

// `parse_literal` (bulk-load helper) must never panic and only returns Some on
// a single clean literal.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = libtau::parse_literal(s);
    }
});
