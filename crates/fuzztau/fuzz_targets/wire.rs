#![no_main]
use libfuzzer_sys::fuzz_target;

// `Response::parse` (wire decoder) must never panic on arbitrary input.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = libtau::Response::parse(s);
    }
});
