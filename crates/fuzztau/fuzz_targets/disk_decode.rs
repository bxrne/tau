#![no_main]
use libfuzzer_sys::fuzz_target;

// Binary deserialisation fuzzer — the highest-risk surface, and the only one of
// these targets that takes *raw* bytes (no UTF-8 gate). It mirrors the on-disk
// `.dat` decode path that a corrupted file flows through.
//
// The payload parser is version-dependent: v1 taus are a bare `[start, end)`
// pair, v2 prefixes a per-tau axis count for N-dimensional lenses. Both parsers
// must be panic-free, so the same bytes are decoded at every supported version
// as well as through the full header + CRC + zstd envelope. `decode_payload_*`
// reaches the interval / arity / length-prefix parsing directly (the code where
// a corrupted file once decoded an inverted `[start, end)` and panicked in
// `Tau::new`); `decode_image_bytes` exercises the header + CRC + zstd envelope.
// No path may panic on any input — only `Ok`/`Err`.
fuzz_target!(|data: &[u8]| {
    // Current on-disk version (v2: per-tau axis count).
    let _ = libtau::Disk::<libtau::Value>::decode_payload_bytes(data);
    // v1 migration read path (bare start/end pairs, no arity byte).
    let _ = libtau::Disk::<libtau::Value>::decode_payload_bytes_versioned(data, 1);
    // v2 path pinned explicitly, so the target keeps covering it if the current
    // version advances past 2.
    let _ = libtau::Disk::<libtau::Value>::decode_payload_bytes_versioned(data, 2);
    // Full header + optional decrypt + zstd + payload; the header carries its
    // own version byte, so this reaches both parsers from a valid image.
    let _ = libtau::Disk::<libtau::Value>::decode_image_bytes(data, None);
});
