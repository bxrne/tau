//! Disk-persisted backend for the store.
//!
//! Binary format:
//!   Header:  MAGIC (3 bytes: "TAU") + VERSION (1 byte) + checksum (4 bytes)
//!   Entries: layer_id (8 bytes, u64) + lens_name_len (4 bytes) + lens_name +
//!            tau_count (4 bytes) + repeated taus
//!            Each tau: start (8 bytes, i64) + end (8 bytes, i64) + value (8 bytes, i64)
//!
//! Checksums cover all bytes after the checksum field itself.

use crc32fast::Hasher;

use crate::libtau::model::{Layer, LayerId, Tau};
use crate::libtau::storage::store::{COMPACT_THRESHOLD, Store, compact_layers};
use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

const MAGIC: &[u8] = b"TAU";
const VERSION: u8 = 1;
const HEADER_OVERHEAD: usize = MAGIC.len() + 1; // + version

/// Compute CRC32 over bytes.
fn checksum(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Serialize a string to bytes with length prefix.
fn write_str<W: Write>(writer: &mut W, s: &str) -> io::Result<()> {
    writer.write_all(&(s.len() as u32).to_le_bytes())?;
    writer.write_all(s.as_bytes())
}

/// Deserialize a string from bytes with length prefix.
fn read_str<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let result: Result<String, _> = String::from_utf8(buf);
    result.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Trait for binary encoding/decoding of values.
pub trait Codec: Sized {
    /// Encode and write value to writer.
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()>;
    /// Read and decode value from reader.
    fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self>;
}

macro_rules! impl_codec_binary {
    ($($t:ty),+) => {
        $(impl Codec for $t {
            fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()> {
                writer.write_all(&(*self as i64).to_le_bytes())
            }
            fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self> {
                let mut buf = [0u8; 8];
                reader.read_exact(&mut buf)?;
                Ok(i64::from_le_bytes(buf) as $t)
            }
        })+
    };
}

impl_codec_binary!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, f32, f64);

impl Codec for bool {
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&(*self as u8).to_le_bytes())
    }
    fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;
        Ok(buf[0] != 0)
    }
}

/// One entry in the disk store.
#[derive(Debug)]
struct DiskEntry<V> {
    layer_id: LayerId,
    lens: String,
    taus: Vec<Tau<V>>,
}

impl<V: Codec> DiskEntry<V> {
    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.layer_id.to_le_bytes())?;
        write_str(writer, &self.lens)?;
        writer.write_all(&(self.taus.len() as u32).to_le_bytes())?;
        for tau in &self.taus {
            writer.write_all(&tau.start.to_le_bytes())?;
            writer.write_all(&tau.end.to_le_bytes())?;
            tau.value.write_encoded(writer)?;
        }
        Ok(())
    }

    fn read(reader: &mut impl Read) -> io::Result<Self> {
        let mut layer_id_buf = [0u8; 8];
        reader.read_exact(&mut layer_id_buf)?;
        let layer_id = u64::from_le_bytes(layer_id_buf);

        let lens = read_str(reader)?;

        let mut count_buf = [0u8; 4];
        reader.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;

        let mut taus = Vec::with_capacity(count);
        for _ in 0..count {
            let mut start_buf = [0u8; 8];
            reader.read_exact(&mut start_buf)?;
            let start = i64::from_le_bytes(start_buf);

            let mut end_buf = [0u8; 8];
            reader.read_exact(&mut end_buf)?;
            let end = i64::from_le_bytes(end_buf);

            let value = V::read_encoded(reader)?;
            taus.push(Tau::new(start, end, value));
        }

        Ok(DiskEntry {
            layer_id,
            lens,
            taus,
        })
    }
}

/// Disk-backed implementation of the Store trait.
pub struct Disk<V> {
    path: std::path::PathBuf,
    lenses: BTreeMap<String, Vec<Layer<V>>>,
    compact_threshold: usize,
}

impl<V: Clone + Codec> Disk<V> {
    /// Open existing store or create new one.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut lenses: BTreeMap<String, Vec<Layer<V>>> = BTreeMap::new();

        if path.exists() {
            let file = std::fs::File::open(&path)?;
            let mut reader = BufReader::new(file);

            // Read and verify header
            let mut header = [0u8; HEADER_OVERHEAD + 4]; // + checksum
            reader.read_exact(&mut header)?;

            let (magic, version, stored_checksum) = (
                &header[0..3],
                header[3],
                u32::from_le_bytes(header[4..8].try_into().unwrap()),
            );

            if magic != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid magic: expected TAU, got {:?}",
                        String::from_utf8_lossy(magic)
                    ),
                ));
            }
            if version != VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported version: expected {}, got {}", VERSION, version),
                ));
            }

            let header_without_checksum = &header[0..HEADER_OVERHEAD];
            let computed_checksum = checksum(header_without_checksum);
            if stored_checksum != computed_checksum {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "header checksum mismatch: expected {}, got {}",
                        stored_checksum, computed_checksum
                    ),
                ));
            }

            // Replay entries
            loop {
                match DiskEntry::read(&mut reader) {
                    Ok(entry) => {
                        lenses
                            .entry(entry.lens.clone())
                            .or_default()
                            .push(Layer::new(entry.layer_id, entry.taus));
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
                }
            }
        }

        Ok(Self {
            path,
            lenses,
            compact_threshold: COMPACT_THRESHOLD,
        })
    }

    /// Create a new store at `path`.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(&path)?;
        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        let header_checksum = checksum(&header);
        header.extend_from_slice(&header_checksum.to_le_bytes());
        file.write_all(&header)?;
        file.sync_data()?;

        Ok(Self {
            path,
            lenses: BTreeMap::new(),
            compact_threshold: COMPACT_THRESHOLD,
        })
    }

    /// Flush all in-memory state to disk.
    pub fn flush(&self) -> io::Result<()> {
        let mut file = std::fs::File::create(&self.path)?;

        // Write header
        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        let header_checksum = checksum(&header);
        header.extend_from_slice(&header_checksum.to_le_bytes());
        file.write_all(&header)?;

        // Write entries
        for (lens_name, layers) in &self.lenses {
            for layer in layers {
                let entry = DiskEntry {
                    layer_id: layer.id,
                    lens: lens_name.clone(),
                    taus: layer
                        .taus
                        .iter()
                        .map(|t| Tau::new(t.start, t.end, t.value.clone()))
                        .collect(),
                };
                DiskEntry::write(&entry, &mut file)?;
            }
        }

        file.sync_data()?;
        Ok(())
    }
}

impl<V: Clone + PartialEq + Send + Sync + 'static> Store<V> for Disk<V> {
    fn append(&mut self, lens: &str, layer: Layer<V>) {
        let layers = self.lenses.entry(lens.to_string()).or_default();
        layers.push(layer);
        if layers.len() > self.compact_threshold {
            compact_layers(layers);
        }
    }

    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
        self.lenses.get(lens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::model::Tau;
    use tempfile::NamedTempFile;

    fn layer(id: u64, items: &[(i64, i64, i32)]) -> Layer<i32> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    #[test]
    fn create_and_append() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = Disk::create(tmp.path()).unwrap();
        store.append("x", layer(1, &[(0, 10, 42)]));
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 10), None);
    }

    #[test]
    fn open_replays_data() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path()).unwrap();
            store.append("x", layer(1, &[(0, 10, 42)]));
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path()).unwrap();
        assert_eq!(store.at("x", 5), Some(42));
    }

    #[test]
    fn multiple_layers_shadow() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path()).unwrap();
            store.append("s", layer(1, &[(0, 20, 1)]));
            store.append("s", layer(2, &[(5, 15, 2)]));
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path()).unwrap();
        assert_eq!(store.at("s", 3), Some(1));
        assert_eq!(store.at("s", 7), Some(2));
        assert_eq!(store.at("s", 17), Some(1));
    }

    #[test]
    fn multiple_lenses_independent() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path()).unwrap();
            store.append("a", layer(1, &[(0, 10, 1)]));
            store.append("b", layer(1, &[(0, 10, 2)]));
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path()).unwrap();
        assert_eq!(store.at("a", 5), Some(1));
        assert_eq!(store.at("b", 5), Some(2));
    }

    #[test]
    fn invalid_magic_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        // Write garbage header
        std::fs::write(tmp.path(), b"GARB").unwrap();
        let result = Disk::<i32>::open(tmp.path());
        assert!(result.is_err());
    }
}
