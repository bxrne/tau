#![no_main]
use libfuzzer_sys::fuzz_target;
use libtau::storage::Codec;

// `Value::decode` (the VAL/RANGE wire segment decoder) must never panic.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = libtau::Value::decode(s);
    }
});
