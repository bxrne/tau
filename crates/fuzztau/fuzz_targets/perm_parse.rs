#![no_main]
use libfuzzer_sys::fuzz_target;

// `Perm::parse` (wire GRANTS + users-file loading) must never panic.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = libtau::Perm::parse(s);
    }
});
