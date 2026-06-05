//! Cartesian **profile matrix** for libtau — add dimensions here instead of new enum variants.

use libtau::storage::COMPACT_THRESHOLD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuiteTier {
    Smoke,
    Standard,
    Nightly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    Memory,
    Wal,
    Disk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compaction {
    Default,
    Stress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encryption {
    Plain,
    Aes256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Direct,
    WirePlain,
    WireTls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    Off,
    On,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileSpec {
    pub storage: Storage,
    pub compaction: Compaction,
    pub encryption: Encryption,
    pub transport: Transport,
    pub auth: Auth,
}

pub const TEST_ENCRYPTION_KEY: [u8; 32] = *b"tau-dst-test-key-32-bytes!!!!!!!";

impl ProfileSpec {
    pub const fn enc_key(self) -> Option<[u8; 32]> {
        match self.encryption {
            Encryption::Plain => None,
            Encryption::Aes256 => Some(TEST_ENCRYPTION_KEY),
        }
    }

    pub const fn compact_threshold(self) -> usize {
        match self.compaction {
            Compaction::Default => COMPACT_THRESHOLD,
            Compaction::Stress => 4,
        }
    }

    pub const fn wal_workload(self) -> bool {
        matches!(self.storage, Storage::Wal)
    }

    pub const fn uses_wal_file(self) -> bool {
        matches!(self.storage, Storage::Wal) && matches!(self.transport, Transport::Direct)
    }

    pub const fn uses_disk_dir(self) -> bool {
        matches!(self.storage, Storage::Disk) && matches!(self.transport, Transport::Direct)
    }

    pub const fn is_wire(self) -> bool {
        matches!(self.transport, Transport::WirePlain | Transport::WireTls)
    }

    pub const fn multi_db(self) -> bool {
        !self.wal_workload()
    }

    pub fn name(self) -> String {
        let storage = match self.storage {
            Storage::Memory => "memory",
            Storage::Wal => "wal",
            Storage::Disk => "disk",
        };
        let compact = match self.compaction {
            Compaction::Default => "default",
            Compaction::Stress => "stress",
        };
        let enc = match self.encryption {
            Encryption::Plain => "plain",
            Encryption::Aes256 => "enc",
        };
        let transport = match self.transport {
            Transport::Direct => "direct",
            Transport::WirePlain => "wire",
            Transport::WireTls => "wire_tls",
        };
        let auth = match self.auth {
            Auth::Off => "noauth",
            Auth::On => "auth",
        };
        let db = if self.multi_db() { "multi" } else { "single" };
        format!("{storage}_{compact}_{enc}_{db}_{transport}_{auth}")
    }

    pub const fn ci_ops(self) -> usize {
        if self.is_wire() {
            800
        } else {
            match self.storage {
                Storage::Memory => 2000,
                Storage::Wal | Storage::Disk => 500,
            }
        }
    }

    pub fn engine_matrix(tier: SuiteTier) -> Vec<ProfileSpec> {
        let mut out = Vec::new();
        for &storage in &[Storage::Memory, Storage::Wal, Storage::Disk] {
            for &compaction in &[Compaction::Default, Compaction::Stress] {
                for &encryption in &[Encryption::Plain, Encryption::Aes256] {
                    if matches!(storage, Storage::Memory)
                        && matches!(encryption, Encryption::Aes256)
                    {
                        continue;
                    }
                    let spec = ProfileSpec {
                        storage,
                        compaction,
                        encryption,
                        transport: Transport::Direct,
                        auth: Auth::Off,
                    };
                    if spec.in_tier(tier) {
                        out.push(spec);
                    }
                }
            }
        }
        out
    }

    /// Wire/TLS over in-memory server (exercises handler + codec, not every storage combo).
    pub fn wire_matrix() -> Vec<ProfileSpec> {
        let base = ProfileSpec {
            storage: Storage::Memory,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: Auth::Off,
        };
        vec![
            ProfileSpec {
                transport: Transport::WirePlain,
                auth: Auth::Off,
                ..base
            },
            ProfileSpec {
                transport: Transport::WireTls,
                auth: Auth::Off,
                ..base
            },
            ProfileSpec {
                transport: Transport::WirePlain,
                auth: Auth::On,
                ..base
            },
        ]
    }

    pub fn all_for_tier(tier: SuiteTier) -> Vec<ProfileSpec> {
        let mut specs = Self::engine_matrix(tier);
        if tier == SuiteTier::Nightly {
            specs.extend(Self::wire_matrix());
        }
        specs.retain(|s| s.in_tier(tier));
        specs
    }

    const fn in_tier(self, tier: SuiteTier) -> bool {
        match tier {
            SuiteTier::Smoke => self.is_smoke_representative(),
            SuiteTier::Standard => matches!(self.transport, Transport::Direct),
            SuiteTier::Nightly => true,
        }
    }

    const fn is_smoke_representative(self) -> bool {
        matches!(
            (
                self.storage,
                self.compaction,
                self.encryption,
                self.transport,
                self.auth
            ),
            (
                Storage::Memory,
                Compaction::Default,
                Encryption::Plain,
                Transport::Direct,
                Auth::Off,
            ) | (
                Storage::Memory,
                Compaction::Stress,
                Encryption::Plain,
                Transport::Direct,
                Auth::Off,
            ) | (
                Storage::Wal,
                Compaction::Default,
                Encryption::Plain,
                Transport::Direct,
                Auth::Off,
            ) | (
                Storage::Wal,
                Compaction::Stress,
                Encryption::Plain,
                Transport::Direct,
                Auth::Off,
            ) | (
                Storage::Disk,
                Compaction::Default,
                Encryption::Plain,
                Transport::Direct,
                Auth::Off,
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_has_five_cells() {
        assert_eq!(ProfileSpec::engine_matrix(SuiteTier::Smoke).len(), 5);
    }

    #[test]
    fn standard_includes_encryption() {
        let specs = ProfileSpec::engine_matrix(SuiteTier::Standard);
        assert!(
            specs
                .iter()
                .any(|p| { p.storage == Storage::Wal && p.encryption == Encryption::Aes256 })
        );
        assert_eq!(specs.len(), 10);
    }

    #[test]
    fn nightly_adds_wire() {
        let specs = ProfileSpec::all_for_tier(SuiteTier::Nightly);
        assert!(specs.iter().any(|p| p.transport == Transport::WireTls));
        assert!(specs.len() >= 13);
    }
}
