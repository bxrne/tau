#![no_main]
use libfuzzer_sys::fuzz_target;

// Binary deserialisation fuzzer — the highest-risk surface, and the only one of
// these targets that takes *raw* bytes (no UTF-8 gate). It mirrors the on-disk
// `.dat` decode path that a corrupted file flows through.
//
// `decode_payload_bytes` reaches the interval / length-prefix parsing directly
// (the code where a corrupted file once decoded an inverted `[start, end)` and
// panicked in `Tau::new`); `decode_image_bytes` exercises the full header + CRC
// + zstd envelope. Neither may panic on any input — only `Ok`/`Err`.
fuzz_target!(|data: &[u8]| {
    let _ = libtau::Disk::<libtau::Value>::decode_payload_bytes(data);
    let _ = libtau::Disk::<libtau::Value>::decode_image_bytes(data, None);
});
