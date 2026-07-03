//! Binary encode/decode contract shared by on-disk formats (`Sstable`'s runs
//! and manifest). Distinct from [`crate::storage::wal::Codec`] (the WAL's
//! text-line codec) — kept as a separate trait of the same name in a
//! different module rather than merged, since the two formats' encodings are
//! unrelated (binary vs. text) and a shared trait would force one to bend
//! toward the other for no benefit.

use std::io::{self, Read, Write};

/// Default zstd compression level for on-disk data. Valid range is 1-22;
/// higher = better ratio, slower.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Trait for binary encoding/decoding of values.
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
