//! Disk-persisted backend for the store.
//!
//! # Binary format
//!
//! Header:  `MAGIC` (3 bytes: `b"TAU"`) + `VERSION` (1 byte: `1`) + CRC32 (4 bytes)
//!
//! Each entry:
//! - `layer_id`: u64 LE
//! - `lens_name_len` (u32 LE) + `lens_name` bytes
//! - `tau_count`: u32 LE
//! - For each tau:
//!   - First tau: `start` as absolute i64 LE; `duration = end - start` as unsigned LEB128.
//!   - Subsequent taus: `start_delta = start - prev_end` as signed LEB128; `duration` as unsigned LEB128.
//!   - Value: fixed-width encoding (8 bytes numeric, 1 byte bool).
//!
//! # Durability
//!
//! `append` writes the new entry to the file (with fsync) before updating
//! the in-memory cache.  `flush()` is a no-op kept for API stability.

use crc32fast::Hasher;

use crate::libtau::model::{Layer, LayerId, Tau, Timestamp};
use crate::libtau::storage::store::Store;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8] = b"TAU";
const VERSION: u8 = 1;
const HEADER_OVERHEAD: usize = MAGIC.len() + 1; // magic + version byte

fn write_leb128_signed<W: Write>(w: &mut W, mut val: i64) -> io::Result<()> {
    loop {
        let byte = (val as u8) & 0x7F;
        let new_val = val >> 7;
        let sign_bit = byte & 0x40;
        let more = !((new_val == 0 && sign_bit == 0) || (new_val == -1 && sign_bit != 0));
        w.write_all(&[if more { byte | 0x80 } else { byte }])?;
        if !more {
            return Ok(());
        }
        val = new_val;
    }
}

fn read_leb128_signed<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut result: i64 = 0;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        result |= ((b & 0x7F) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            if shift < 64 && (b & 0x40) != 0 {
                result |= !0i64 << shift;
            }
            return Ok(result);
        }
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LEB128 signed overflow",
            ));
        }
    }
}

fn write_leb128_unsigned<W: Write>(w: &mut W, mut val: u64) -> io::Result<()> {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            w.write_all(&[byte])?;
            return Ok(());
        }
        w.write_all(&[byte | 0x80])?;
    }
}

fn read_leb128_unsigned<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LEB128 unsigned overflow",
            ));
        }
    }
}

fn checksum(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

fn write_str<W: Write>(writer: &mut W, s: &str) -> io::Result<()> {
    writer.write_all(&(s.len() as u32).to_le_bytes())?;
    writer.write_all(s.as_bytes())
}

fn read_str<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Binary encoding/decoding of values stored in the disk format.
pub trait Codec: Sized {
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()>;
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
        let mut prev_end: Option<Timestamp> = None;
        for tau in &self.taus {
            match prev_end {
                None => writer.write_all(&tau.start.to_le_bytes())?,
                Some(p) => write_leb128_signed(writer, tau.start - p)?,
            }
            write_leb128_unsigned(writer, (tau.end - tau.start) as u64)?;
            tau.value.write_encoded(writer)?;
            prev_end = Some(tau.end);
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
        let mut prev_end: Option<Timestamp> = None;
        for _ in 0..count {
            let start = match prev_end {
                None => {
                    let mut buf = [0u8; 8];
                    reader.read_exact(&mut buf)?;
                    i64::from_le_bytes(buf)
                }
                Some(p) => p + read_leb128_signed(reader)?,
            };
            let duration = read_leb128_unsigned(reader)? as i64;
            let value = V::read_encoded(reader)?;
            let end = start + duration;
            taus.push(Tau::new(start, end, value));
            prev_end = Some(end);
        }
        Ok(DiskEntry {
            layer_id,
            lens,
            taus,
        })
    }
}

fn write_header<W: Write>(w: &mut W) -> io::Result<()> {
    let mut header = Vec::with_capacity(HEADER_OVERHEAD + 4);
    header.extend_from_slice(MAGIC);
    header.push(VERSION);
    let crc = checksum(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    w.write_all(&header)
}

/// Disk-backed implementation of the Store trait.
pub struct Disk<V> {
    path: std::path::PathBuf,
    lenses: BTreeMap<String, Vec<Layer<V>>>,
}

impl<V: Clone + Codec> Disk<V> {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut lenses: BTreeMap<String, Vec<Layer<V>>> = BTreeMap::new();

        if path.exists() {
            let file = std::fs::File::open(&path)?;
            let mut reader = BufReader::new(file);

            let mut header = [0u8; HEADER_OVERHEAD + 4];
            reader.read_exact(&mut header)?;

            let magic = &header[0..3];
            let ver = header[3];
            let stored_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());

            if magic != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid magic: expected TAU, got {:?}",
                        String::from_utf8_lossy(magic)
                    ),
                ));
            }
            if ver != VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported disk version: {ver}"),
                ));
            }
            if stored_crc != checksum(&header[0..HEADER_OVERHEAD]) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "header checksum mismatch",
                ));
            }

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

        Ok(Self { path, lenses })
    }

    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&path)?;
        write_header(&mut file)?;
        file.sync_data()?;
        Ok(Self {
            path,
            lenses: BTreeMap::new(),
        })
    }

    /// No-op — `append` now writes immediately on every call.
    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    fn rewrite_all(&self) -> io::Result<()> {
        let mut file = std::fs::File::create(&self.path)?;
        write_header(&mut file)?;
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
                entry.write(&mut file)?;
            }
        }
        file.sync_data()?;
        Ok(())
    }

    fn append_to_file(&self, lens: &str, layer: &Layer<V>) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let mut writer = BufWriter::new(file);
        let entry = DiskEntry {
            layer_id: layer.id,
            lens: lens.to_string(),
            taus: layer
                .taus
                .iter()
                .map(|t| Tau::new(t.start, t.end, t.value.clone()))
                .collect(),
        };
        entry.write(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
        Ok(())
    }
}

impl<V: Clone + Codec + Send + Sync + 'static> Store<V> for Disk<V> {
    fn append(&mut self, lens: &str, layer: Layer<V>) {
        if let Err(e) = self.append_to_file(lens, &layer) {
            panic!("disk append failed: {e}");
        }
        let layers = self.lenses.entry(lens.to_string()).or_default();
        let pos = layers.partition_point(|l| l.id < layer.id);
        layers.insert(pos, layer);
    }

    fn replace_layers(&mut self, lens: &str, layer: Layer<V>) {
        self.lenses.insert(lens.to_string(), vec![layer]);
        if let Err(e) = self.rewrite_all() {
            panic!("disk rewrite failed: {e}");
        }
    }

    fn at(&self, lens: &str, t: Timestamp) -> Option<V> {
        self.lenses
            .get(lens)?
            .iter()
            .rev()
            .filter(|l| l.min_ts <= t && t < l.max_ts)
            .find_map(|layer| layer.at(t))
            .cloned()
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
        }
        let store = Disk::open(tmp.path()).unwrap();
        assert_eq!(store.at("a", 5), Some(1));
        assert_eq!(store.at("b", 5), Some(2));
    }

    #[test]
    fn invalid_magic_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"GARB").unwrap();
        assert!(Disk::<i32>::open(tmp.path()).is_err());
    }

    #[test]
    fn block_skip_at() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = Disk::create(tmp.path()).unwrap();
        store.append("x", layer(1, &[(0, 10, 1)]));
        store.append("x", layer(2, &[(20, 30, 2)]));
        assert_eq!(store.at("x", 5), Some(1));
        assert_eq!(store.at("x", 25), Some(2));
        assert_eq!(store.at("x", 15), None); // gap
    }
}
