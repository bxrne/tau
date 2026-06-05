//! Test / smoke fixtures.

use super::{Auth, Compaction, Encryption, ProfileSpec, Storage, Transport};

#[cfg(test)]
pub const MEMORY_MULTI: ProfileSpec = ProfileSpec {
    storage: Storage::Memory,
    compaction: Compaction::Default,
    encryption: Encryption::Plain,
    transport: Transport::Direct,
    auth: Auth::Off,
};
