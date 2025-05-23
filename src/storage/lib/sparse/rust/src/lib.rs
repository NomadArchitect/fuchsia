// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(target_endian = "big")]
assert!(false, "This library assumes little-endian!");

pub mod builder;
mod format;
pub mod reader;

use crate::format::{ChunkHeader, SparseHeader};
use anyhow::{bail, ensure, Context, Result};
use core::fmt;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use tempfile::{NamedTempFile, TempPath};

// Size of blocks to write.  Note that the format supports varied block sizes; this is the preferred
// size by this library.
const BLK_SIZE: u32 = 0x1000;

fn deserialize_from<'a, T: DeserializeOwned, R: Read + ?Sized>(source: &mut R) -> Result<T> {
    let mut buf = vec![0u8; std::mem::size_of::<T>()];
    source.read_exact(&mut buf[..]).context("Failed to read bytes")?;
    Ok(bincode::deserialize(&buf[..])?)
}

/// A union trait for `Write` and `Seek` that also allows truncation.
pub trait Writer: Write + Seek {
    /// Sets the length of the output stream.
    fn set_len(&mut self, size: u64) -> Result<()>;
}

impl Writer for File {
    fn set_len(&mut self, size: u64) -> Result<()> {
        Ok(File::set_len(self, size)?)
    }
}

impl Writer for Cursor<Vec<u8>> {
    fn set_len(&mut self, size: u64) -> Result<()> {
        Vec::resize(self.get_mut(), size as usize, 0u8);
        Ok(())
    }
}

// A wrapper around a Reader, which makes it seem like the underlying stream is only self.1 bytes
// long.  The underlying reader is still advanced upon reading.
// This is distinct from `std::io::Take` in that it does not modify the seek offset of the
// underlying reader.  In other words, `LimitedReader` can be used to read a window within the
// reader (by setting seek offset to the start, and the size limit to the end).
struct LimitedReader<'a, R>(pub &'a mut R, pub usize);

impl<'a, R: Read + Seek> Read for LimitedReader<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let offset = self.0.stream_position()?;
        let avail = self.1.saturating_sub(offset as usize);
        let to_read = std::cmp::min(avail, buf.len());
        self.0.read(&mut buf[..to_read])
    }
}

/// Returns whether the image in `reader` appears to be in the sparse format.
pub fn is_sparse_image<R: Read + Seek>(reader: &mut R) -> bool {
    || -> Option<bool> {
        let header: SparseHeader = deserialize_from(reader).ok()?;
        let is_sparse = header.magic == format::SPARSE_HEADER_MAGIC;
        reader.seek(SeekFrom::Start(0)).ok()?;
        Some(is_sparse)
    }()
    .unwrap_or(false)
}

#[derive(Clone, PartialEq, Debug)]
enum Chunk {
    /// `Raw` represents a set of blocks to be written to disk as-is.
    /// `start` is the offset in the expanded image at which the Raw section starts.
    /// `start` and `size` are in bytes, but must be block-aligned.
    Raw { start: u64, size: u32 },
    /// `Fill` represents a Chunk that has the `value` repeated enough to fill `size` bytes.
    /// `start` is the offset in the expanded image at which the Fill section starts.
    /// `start` and `size` are in bytes, but must be block-aligned.
    Fill { start: u64, size: u32, value: u32 },
    /// `DontCare` represents a set of blocks that need to be "offset" by the
    /// image recipient.  If an image needs to be broken up into two sparse images, and we flash n
    /// bytes for Sparse Image 1, Sparse Image 2 needs to start with a DontCareChunk with
    /// (n/blocksize) blocks as its "size" property.
    /// `start` is the offset in the expanded image at which the DontCare section starts.
    /// `start` and `size` are in bytes, but must be block-aligned.
    DontCare { start: u64, size: u32 },
    /// `Crc32Chunk` is used as a checksum of a given set of Chunks for a SparseImage.  This is not
    /// required and unused in most implementations of the Sparse Image format. The type is included
    /// for completeness. It has 4 bytes of CRC32 checksum as describable in a u32.
    #[allow(dead_code)]
    Crc32 { checksum: u32 },
}

impl Chunk {
    /// Attempts to read a `Chunk` from `reader`.  The reader will be positioned at the first byte
    /// following the chunk header and any extra data; for a Raw chunk this means it will point at
    /// the data payload, and for other chunks it will point at the next chunk header (or EOF).
    /// `offset` is the current offset in the logical volume.
    pub fn read_metadata<R: Read>(reader: &mut R, offset: u64, block_size: u32) -> Result<Self> {
        let header: ChunkHeader =
            deserialize_from(reader).context("Failed to read chunk header")?;
        ensure!(header.valid(), "Invalid chunk header");

        let size = header
            .chunk_sz
            .checked_mul(block_size)
            .context("Chunk size * block size can not be larger than 2^32")?;
        match header.chunk_type {
            format::CHUNK_TYPE_RAW => Ok(Self::Raw { start: offset, size }),
            format::CHUNK_TYPE_FILL => {
                let value: u32 =
                    deserialize_from(reader).context("Failed to deserialize fill value")?;
                Ok(Self::Fill { start: offset, size, value })
            }
            format::CHUNK_TYPE_DONT_CARE => Ok(Self::DontCare { start: offset, size }),
            format::CHUNK_TYPE_CRC32 => {
                let checksum: u32 =
                    deserialize_from(reader).context("Failed to deserialize checksum")?;
                Ok(Self::Crc32 { checksum })
            }
            // We already validated the chunk_type in `ChunkHeader::is_valid`.
            _ => unreachable!(),
        }
    }

    fn valid(&self, block_size: u32) -> bool {
        self.output_size() % block_size == 0
    }

    /// Returns the offset into the logical image the chunk refers to, or None if the chunk has no
    /// output data.
    fn output_offset(&self) -> Option<u64> {
        match self {
            Self::Raw { start, .. } => Some(*start),
            Self::Fill { start, .. } => Some(*start),
            Self::DontCare { start, .. } => Some(*start),
            Self::Crc32 { .. } => None,
        }
    }

    /// Return number of bytes the chunk expands to when written to the partition.
    fn output_size(&self) -> u32 {
        match self {
            Self::Raw { size, .. } => *size,
            Self::Fill { size, .. } => *size,
            Self::DontCare { size, .. } => *size,
            Self::Crc32 { .. } => 0,
        }
    }

    /// Return number of blocks the chunk expands to when written to the partition.
    fn output_blocks(&self, block_size: u32) -> u32 {
        self.output_size().div_ceil(block_size)
    }

    /// `chunk_type` returns the integer flag to represent the type of chunk
    /// to use in the ChunkHeader
    fn chunk_type(&self) -> u16 {
        match self {
            Self::Raw { .. } => format::CHUNK_TYPE_RAW,
            Self::Fill { .. } => format::CHUNK_TYPE_FILL,
            Self::DontCare { .. } => format::CHUNK_TYPE_DONT_CARE,
            Self::Crc32 { .. } => format::CHUNK_TYPE_CRC32,
        }
    }

    /// `chunk_data_len` returns the length of the chunk's header plus the
    /// length of the data when serialized
    fn chunk_data_len(&self) -> u32 {
        let header_size = format::CHUNK_HEADER_SIZE;
        let data_size = match self {
            Self::Raw { size, .. } => *size,
            Self::Fill { .. } => std::mem::size_of::<u32>() as u32,
            Self::DontCare { .. } => 0,
            Self::Crc32 { .. } => std::mem::size_of::<u32>() as u32,
        };
        header_size.checked_add(data_size).unwrap()
    }

    /// Writes the chunk to the given Writer.  `source` is a Reader containing the data payload for
    /// a Raw type chunk, with the seek offset pointing to the first byte of the data payload, and
    /// with exactly enough bytes available for the rest of the data payload.
    fn write<W: Write, R: Read>(
        &self,
        source: Option<&mut R>,
        dest: &mut W,
        block_size: u32,
    ) -> Result<()> {
        ensure!(self.valid(block_size), "Not writing invalid chunk",);
        let header = ChunkHeader::new(
            self.chunk_type(),
            0x0,
            self.output_blocks(block_size),
            self.chunk_data_len(),
        );

        bincode::serialize_into(&mut *dest, &header)?;

        match self {
            Self::Raw { size, .. } => {
                ensure!(source.is_some(), "No source for Raw chunk");
                let n = std::io::copy(source.unwrap(), dest)?;
                let size = *size as u64;
                if n < size {
                    let zeroes = vec![0u8; (size - n) as usize];
                    dest.write_all(&zeroes)?;
                }
            }
            Self::Fill { value, .. } => {
                // Serialize the value,
                bincode::serialize_into(dest, value)?;
            }
            Self::DontCare { .. } => {
                // DontCare has no data to write
            }
            Self::Crc32 { checksum } => {
                bincode::serialize_into(dest, checksum)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Raw { start, size } => {
                format!("RawChunk: start: {}, total bytes: {}", start, size)
            }
            Self::Fill { start, size, value } => {
                format!("FillChunk: start: {}, value: {}, n_blocks: {}", start, value, size)
            }
            Self::DontCare { start, size } => {
                format!("DontCareChunk: start: {}, bytes: {}", start, size)
            }
            Self::Crc32 { checksum } => format!("Crc32Chunk: checksum: {:?}", checksum),
        };
        write!(f, "{}", message)
    }
}

/// Chunk::write takes an Option of something that implements Read. The compiler still requires a
/// concrete type for the generic argument even when the Option is None. This constant can be used
/// in place of None to avoid having to specify a type for the source.
pub const NO_SOURCE: Option<&mut Cursor<&[u8]>> = None;

#[derive(Clone, Debug, PartialEq)]
struct SparseFileWriter {
    chunks: Vec<Chunk>,
}

impl SparseFileWriter {
    fn new(chunks: Vec<Chunk>) -> SparseFileWriter {
        SparseFileWriter { chunks }
    }

    fn total_blocks(&self) -> u32 {
        self.chunks.iter().map(|c| c.output_blocks(BLK_SIZE)).sum()
    }

    fn total_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.output_size() as u64).sum()
    }

    fn write<W: Write + Seek, R: Read + Seek>(&self, reader: &mut R, writer: &mut W) -> Result<()> {
        let header = SparseHeader::new(
            BLK_SIZE.try_into().unwrap(),          // Size of the blocks
            self.total_blocks(),                   // Total blocks in this image
            self.chunks.len().try_into().unwrap(), // Total chunks in this image
        );

        bincode::serialize_into(&mut *writer, &header)?;

        for chunk in &self.chunks {
            let mut reader = if let &Chunk::Raw { start, size } = chunk {
                reader.seek(SeekFrom::Start(start))?;
                Some(LimitedReader(reader, start as usize + size as usize))
            } else {
                None
            };
            chunk.write(reader.as_mut(), writer, BLK_SIZE)?;
        }

        Ok(())
    }
}

impl fmt::Display for SparseFileWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, r"SparseFileWriter: {} Chunks:", self.chunks.len())
    }
}

/// `add_sparse_chunk` takes the input vec, v and given `Chunk`, chunk, and
/// attempts to add the chunk to the end of the vec. If the current last chunk
/// is the same kind of Chunk as the `chunk`, then it will merge the two chunks
/// into one chunk.
///
/// Example: A `FillChunk` with value 0 and size 1 is the last chunk
/// in `v`, and `chunk` is a FillChunk with value 0 and size 1, after this,
/// `v`'s last element will be a FillChunk with value 0 and size 2.
fn add_sparse_chunk(r: &mut Vec<Chunk>, chunk: Chunk) -> Result<()> {
    match r.last_mut() {
        // We've got something in the Vec... if they are both the same type,
        // merge them, otherwise, just push the new one
        Some(last) => match (&last, &chunk) {
            (Chunk::Raw { start, size }, Chunk::Raw { size: new_length, .. })
                if size.checked_add(*new_length).is_some() =>
            {
                *last = Chunk::Raw { start: *start, size: size + new_length };
                return Ok(());
            }
            (
                Chunk::Fill { start, size, value },
                Chunk::Fill { size: new_size, value: new_value, .. },
            ) if value == new_value && size.checked_add(*new_size).is_some() => {
                *last = Chunk::Fill { start: *start, size: size + new_size, value: *value };
                return Ok(());
            }
            (Chunk::DontCare { start, size }, Chunk::DontCare { size: new_size, .. })
                if size.checked_add(*new_size).is_some() =>
            {
                *last = Chunk::DontCare { start: *start, size: size + new_size };
                return Ok(());
            }
            _ => {}
        },
        None => {}
    }

    // If the chunk types differ they cannot be merged.
    // If they are both Fill but have different values, they cannot be merged.
    // Crc32 cannot be merged.
    // If we don't have any chunks then we add it
    r.push(chunk);
    Ok(())
}

/// Reads a sparse image from `source` and expands it to its unsparsed representation in `dest`.
pub fn unsparse<W: Writer, R: Read + Seek>(source: &mut R, dest: &mut W) -> Result<()> {
    let header: SparseHeader = deserialize_from(source).context("Failed to read header")?;
    ensure!(header.valid(), "Invalid sparse image header {:?}", header);

    for _ in 0..header.total_chunks {
        expand_chunk(source, dest, header.blk_sz).context("Failed to expand chunk")?;
    }
    // Truncate output to its current seek offset, in case the last chunk we wrote was DontNeed.
    let offset = dest.stream_position()?;
    dest.set_len(offset).context("Failed to truncate output")?;
    dest.flush()?;
    Ok(())
}

/// Reads a chunk from `source`, and expands it, writing the result to `dest`.
fn expand_chunk<R: Read + Seek, W: Write + Seek>(
    source: &mut R,
    dest: &mut W,
    block_size: u32,
) -> Result<()> {
    let header: ChunkHeader =
        deserialize_from(source).context("Failed to deserialize chunk header")?;
    ensure!(header.valid(), "Invalid chunk header {:x?}", header);
    let size = (header.chunk_sz * block_size) as usize;
    match header.chunk_type {
        format::CHUNK_TYPE_RAW => {
            let limit = source.stream_position()? as usize + size;
            std::io::copy(&mut LimitedReader(source, limit), dest)
                .context("Failed to copy contents")?;
        }
        format::CHUNK_TYPE_FILL => {
            let value: [u8; 4] =
                deserialize_from(source).context("Failed to deserialize fill value")?;
            assert!(size % 4 == 0);
            let repeated = value.repeat(size / 4);
            dest.write_all(&repeated).context("Failed to fill contents")?;
        }
        format::CHUNK_TYPE_DONT_CARE => {
            dest.seek(SeekFrom::Current(size as i64)).context("Failed to skip contents")?;
        }
        format::CHUNK_TYPE_CRC32 => {
            let _: u32 = deserialize_from(source).context("Failed to deserialize fill value")?;
        }
        _ => bail!("Invalid type {}", header.chunk_type),
    };
    Ok(())
}

/// `resparse` takes a SparseFile and a maximum size and will
/// break the single SparseFile into multiple SparseFiles whose
/// size will not exceed the maximum_download_size.
///
/// This will return an error if max_download_size is <= BLK_SIZE
fn resparse(
    sparse_file: SparseFileWriter,
    max_download_size: u64,
) -> Result<Vec<SparseFileWriter>> {
    if max_download_size <= BLK_SIZE as u64 {
        anyhow::bail!(
            "Given maximum download size ({}) is less than the block size ({})",
            max_download_size,
            BLK_SIZE
        );
    }
    let mut ret = Vec::<SparseFileWriter>::new();

    // File length already starts with a header for the SparseFile as
    // well as the size of a potential DontCare and Crc32 Chunk
    let sunk_file_length = format::SPARSE_HEADER_SIZE as u64
        + Chunk::DontCare { start: 0, size: BLK_SIZE }.chunk_data_len() as u64
        + Chunk::Crc32 { checksum: 2345 }.chunk_data_len() as u64;

    let mut chunk_pos = 0;
    let mut output_offset = 0;
    while chunk_pos < sparse_file.chunks.len() {
        log::trace!("Starting a new file at chunk position: {}", chunk_pos);

        let mut file_len = 0;
        file_len += sunk_file_length;

        let mut chunks = Vec::<Chunk>::new();
        if chunk_pos > 0 {
            // If we already have some chunks... add a DontCare block to
            // move the pointer
            log::trace!("Adding a DontCare chunk offset: {}", chunk_pos);
            let dont_care = Chunk::DontCare { start: 0, size: output_offset.try_into().unwrap() };
            chunks.push(dont_care);
        }

        loop {
            match sparse_file.chunks.get(chunk_pos) {
                Some(chunk) => {
                    let curr_chunk_data_len = chunk.chunk_data_len() as u64;
                    if (file_len + curr_chunk_data_len) > max_download_size {
                        log::trace!(
                            "Current file size is: {} and adding another chunk of len: {} would \
                             put us over our max: {}",
                            file_len,
                            curr_chunk_data_len,
                            max_download_size
                        );

                        // Add a don't care chunk to cover everything to the end of the image. While
                        // this is not strictly speaking needed, other tools (simg2simg) produce
                        // this chunk, and the Sparse image inspection tool simg_dump will produce a
                        // warning if a sparse file does not have the same number of output blocks
                        // as declared in the header.
                        let remainder_size = sparse_file.total_bytes() - output_offset;
                        let dont_care = Chunk::DontCare {
                            start: output_offset,
                            size: remainder_size.try_into().unwrap(),
                        };
                        chunks.push(dont_care);
                        break;
                    }
                    log::trace!(
                        "chunk: {} curr_chunk_data_len: {} current file size: {} \
                         max_download_size: {} diff: {}",
                        chunk_pos,
                        curr_chunk_data_len,
                        file_len,
                        max_download_size,
                        (max_download_size - file_len - curr_chunk_data_len)
                    );
                    add_sparse_chunk(&mut chunks, chunk.clone())?;
                    file_len += curr_chunk_data_len;
                    chunk_pos = chunk_pos + 1;
                    output_offset += chunk.output_size() as u64;
                }
                None => {
                    log::trace!("Finished iterating chunks");
                    break;
                }
            }
        }
        let resparsed = SparseFileWriter::new(chunks);
        log::trace!("resparse: Adding new SparseFile: {}", resparsed);
        ret.push(resparsed);
    }

    Ok(ret)
}

/// Takes the given `file_to_upload` for the `named` partition and creates a
/// set of temporary files in the given `dir` in Sparse Image Format. With the
/// provided `max_download_size` constraining file size.
///
/// # Arguments
///
/// * `name` - Name of the partition the image. Used for logs only.
/// * `file_to_upload` - Path to the file to translate to sparse image format.
/// * `dir` - Path to write the Sparse file(s).
/// * `max_download_size` - Maximum size that can be downloaded by the device.
pub fn build_sparse_files(
    name: &str,
    file_to_upload: &str,
    dir: &Path,
    max_download_size: u64,
) -> Result<Vec<TempPath>> {
    if max_download_size <= BLK_SIZE as u64 {
        anyhow::bail!(
            "Given maximum download size ({}) is less than the block size ({})",
            max_download_size,
            BLK_SIZE
        );
    }
    log::debug!("Building sparse files for: {}. File: {}", name, file_to_upload);
    let mut in_file = File::open(file_to_upload)?;

    let mut total_read: usize = 0;
    // Preallocate vector to avoid reallocations as it grows.
    let mut chunks =
        Vec::<Chunk>::with_capacity((in_file.metadata()?.len() as usize / BLK_SIZE as usize) + 1);
    let mut buf = [0u8; BLK_SIZE as usize];
    loop {
        let read = in_file.read(&mut buf)?;
        if read == 0 {
            break;
        }

        let is_fill = buf.chunks(4).collect::<Vec<&[u8]>>().windows(2).all(|w| w[0] == w[1]);
        if is_fill {
            // The Android Sparse Image Format specifies that a fill block
            // is a four-byte u32 repeated to fill BLK_SIZE. Here we use
            // bincode::deserialize to get the repeated four byte pattern from
            // the buffer so that it can be serialized later when we write
            // the sparse file with bincode::serialize.
            let value: u32 = bincode::deserialize(&buf[0..4])?;
            // Add a fill chunk
            let fill = Chunk::Fill {
                start: total_read as u64,
                size: buf.len().try_into().unwrap(),
                value,
            };
            log::trace!("Sparsing file: {}. Created: {}", file_to_upload, fill);
            chunks.push(fill);
        } else {
            // Add a raw chunk
            let raw = Chunk::Raw { start: total_read as u64, size: buf.len().try_into().unwrap() };
            log::trace!("Sparsing file: {}. Created: {}", file_to_upload, raw);
            chunks.push(raw);
            if read < buf.len() {
                // We've reached the end of the file add a DontCare chunk to
                // skip the last bit of the file which is zeroed out from the previous
                // raw buffer
                let skip_end =
                    Chunk::DontCare { start: (total_read + read) as u64, size: BLK_SIZE };
                chunks.push(skip_end);
            }
        }
        total_read += read;
    }

    log::trace!("Creating sparse file from: {} chunks", chunks.len());

    // At this point we are making a new sparse file fom an unoptimized set of
    // Chunks. This primarily means that adjacent Fill chunks of same value are
    // not collapsed into a single Fill chunk (with a larger size). The advantage
    // to this two pass approach is that (with some future work), we can create
    // the "unoptimized" sparse file from a given image, and then "resparse" it
    // as many times as desired with different `max_download_size` parameters.
    // This would simplify the scenario where we want to flash the same image
    // to multiple physical devices which may have slight differences in their
    // hardware (and therefore different `max_download_size`es)
    let sparse_file = SparseFileWriter::new(chunks);
    log::trace!("Created sparse file: {}", sparse_file);

    let mut ret = Vec::<TempPath>::new();
    log::trace!("Resparsing sparse file");
    for re_sparsed_file in resparse(sparse_file, max_download_size)? {
        let (file, temp_path) = NamedTempFile::new_in(dir)?.into_parts();
        let mut file_create = File::from(file);

        log::trace!("Writing resparsed {} to disk", re_sparsed_file);
        re_sparsed_file.write(&mut in_file, &mut file_create)?;

        ret.push(temp_path);
    }

    log::debug!("Finished building sparse files");

    Ok(ret)
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    #[cfg(target_os = "linux")]
    use crate::build_sparse_files;

    use super::builder::{DataSource, SparseImageBuilder};
    use super::{
        add_sparse_chunk, resparse, unsparse, Chunk, SparseFileWriter, BLK_SIZE, NO_SOURCE,
    };
    use rand::rngs::SmallRng;
    use rand::{RngCore, SeedableRng};
    use std::io::{Cursor, Read as _, Seek as _, SeekFrom, Write as _};
    #[cfg(target_os = "linux")]
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::process::{Command, Stdio};
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_fill_into_bytes() {
        let mut dest = Cursor::new(Vec::<u8>::new());

        let fill_chunk = Chunk::Fill { start: 0, size: 5 * BLK_SIZE, value: 365 };
        fill_chunk.write(NO_SOURCE, &mut dest, BLK_SIZE).unwrap();
        assert_eq!(dest.into_inner(), [194, 202, 0, 0, 5, 0, 0, 0, 16, 0, 0, 0, 109, 1, 0, 0]);
    }

    #[test]
    fn test_raw_into_bytes() {
        const EXPECTED_RAW_BYTES: [u8; 22] =
            [193, 202, 0, 0, 1, 0, 0, 0, 12, 16, 0, 0, 49, 50, 51, 52, 53, 0, 0, 0, 0, 0];

        let mut source = Cursor::new(Vec::<u8>::from(&b"12345"[..]));
        let mut sparse = Cursor::new(Vec::<u8>::new());
        let chunk = Chunk::Raw { start: 0, size: BLK_SIZE };

        chunk.write(Some(&mut source), &mut sparse, BLK_SIZE).unwrap();
        let buf = sparse.into_inner();
        assert_eq!(buf.len(), 4108);
        assert_eq!(&buf[..EXPECTED_RAW_BYTES.len()], EXPECTED_RAW_BYTES);
        assert_eq!(&buf[EXPECTED_RAW_BYTES.len()..], &[0u8; 4108 - EXPECTED_RAW_BYTES.len()]);
    }

    #[test]
    fn test_dont_care_into_bytes() {
        let mut dest = Cursor::new(Vec::<u8>::new());
        let chunk = Chunk::DontCare { start: 0, size: 5 * BLK_SIZE };

        chunk.write(NO_SOURCE, &mut dest, BLK_SIZE).unwrap();
        assert_eq!(dest.into_inner(), [195, 202, 0, 0, 5, 0, 0, 0, 12, 0, 0, 0]);
    }

    #[test]
    fn test_sparse_file_into_bytes() {
        let mut source = Cursor::new(Vec::<u8>::from(&b"123"[..]));
        let mut sparse = Cursor::new(Vec::<u8>::new());
        let mut chunks = Vec::<Chunk>::new();
        // Add a fill chunk
        let fill = Chunk::Fill { start: 0, size: 4096, value: 5 };
        chunks.push(fill);
        // Add a raw chunk
        let raw = Chunk::Raw { start: 0, size: 12288 };
        chunks.push(raw);
        // Add a dontcare chunk
        let dontcare = Chunk::DontCare { start: 0, size: 4096 };
        chunks.push(dontcare);

        let sparsefile = SparseFileWriter::new(chunks);
        sparsefile.write(&mut source, &mut sparse).unwrap();

        sparse.seek(SeekFrom::Start(0)).unwrap();
        let mut unsparsed = Cursor::new(Vec::<u8>::new());
        unsparse(&mut sparse, &mut unsparsed).unwrap();
        let buf = unsparsed.into_inner();
        assert_eq!(buf.len(), 4096 + 12288 + 4096);
        {
            let chunks = buf[..4096].chunks(4);
            for chunk in chunks {
                assert_eq!(chunk, &[5u8, 0, 0, 0]);
            }
        }
        assert_eq!(&buf[4096..4099], b"123");
        assert_eq!(&buf[4099..16384], &[0u8; 12285]);
        assert_eq!(&buf[16384..], &[0u8; 4096]);
    }

    ////////////////////////////////////////////////////////////////////////////
    // Tests for resparse

    #[test]
    fn test_resparse_bails_on_too_small_size() {
        let sparse = SparseFileWriter::new(Vec::<Chunk>::new());
        assert!(resparse(sparse, 4095).is_err());
    }

    #[test]
    fn test_resparse_splits() {
        let max_download_size = 4096 * 2;

        let mut chunks = Vec::<Chunk>::new();
        chunks.push(Chunk::Raw { start: 0, size: 4096 });
        chunks.push(Chunk::Fill { start: 4096, size: 4096, value: 2 });
        // We want 2 sparse files with the second sparse file having a
        // DontCare chunk and then this chunk
        chunks.push(Chunk::Raw { start: 8192, size: 4096 });

        let input_sparse_file = SparseFileWriter::new(chunks);
        let resparsed_files = resparse(input_sparse_file, max_download_size).unwrap();
        assert_eq!(2, resparsed_files.len());

        assert_eq!(3, resparsed_files[0].chunks.len());
        assert_eq!(Chunk::Raw { start: 0, size: 4096 }, resparsed_files[0].chunks[0]);
        assert_eq!(Chunk::Fill { start: 4096, size: 4096, value: 2 }, resparsed_files[0].chunks[1]);
        assert_eq!(Chunk::DontCare { start: 8192, size: 4096 }, resparsed_files[0].chunks[2]);

        assert_eq!(2, resparsed_files[1].chunks.len());
        assert_eq!(Chunk::DontCare { start: 0, size: 8192 }, resparsed_files[1].chunks[0]);
        assert_eq!(Chunk::Raw { start: 8192, size: 4096 }, resparsed_files[1].chunks[1]);
    }

    ////////////////////////////////////////////////////////////////////////////
    // Tests for add_sparse_chunk

    #[test]
    fn test_add_sparse_chunk_adds_empty() {
        let init_vec = Vec::<Chunk>::new();
        let mut res = init_vec.clone();
        add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: 4096, value: 1 }).unwrap();
        assert_eq!(0, init_vec.len());
        assert_ne!(init_vec, res);
        assert_eq!(Chunk::Fill { start: 0, size: 4096, value: 1 }, res[0]);
    }

    #[test]
    fn test_add_sparse_chunk_fill() {
        // Test they merge.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Fill { start: 0, size: 8192, value: 1 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: 8192, value: 1 }).unwrap();
            assert_eq!(1, res.len());
            assert_eq!(Chunk::Fill { start: 0, size: 16384, value: 1 }, res[0]);
        }

        // Test don't merge on different value.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Fill { start: 0, size: 4096, value: 1 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: 4096, value: 2 }).unwrap();
            assert_ne!(res, init_vec);
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::Fill { start: 0, size: 4096, value: 1 },
                    Chunk::Fill { start: 0, size: 4096, value: 2 }
                ]
            );
        }

        // Test don't merge on different type.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Fill { start: 0, size: 4096, value: 2 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::DontCare { start: 0, size: 4096 }).unwrap();
            assert_ne!(res, init_vec);
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::Fill { start: 0, size: 4096, value: 2 },
                    Chunk::DontCare { start: 0, size: 4096 }
                ]
            );
        }

        // Test don't merge when too large.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Fill { start: 0, size: 4096, value: 1 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: u32::MAX - 4095, value: 1 })
                .unwrap();
            assert_ne!(res, init_vec);
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::Fill { start: 0, size: 4096, value: 1 },
                    Chunk::Fill { start: 0, size: u32::MAX - 4095, value: 1 }
                ]
            );
        }
    }

    #[test]
    fn test_add_sparse_chunk_dont_care() {
        // Test they merge.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::DontCare { start: 0, size: 4096 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::DontCare { start: 0, size: 4096 }).unwrap();
            assert_eq!(1, res.len());
            assert_eq!(Chunk::DontCare { start: 0, size: 8192 }, res[0]);
        }

        // Test they don't merge on different type.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::DontCare { start: 0, size: 4096 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: 4096, value: 1 }).unwrap();
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::DontCare { start: 0, size: 4096 },
                    Chunk::Fill { start: 0, size: 4096, value: 1 }
                ]
            );
        }

        // Test they don't merge when too large.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::DontCare { start: 0, size: 4096 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::DontCare { start: 0, size: u32::MAX - 4095 })
                .unwrap();
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::DontCare { start: 0, size: 4096 },
                    Chunk::DontCare { start: 0, size: u32::MAX - 4095 }
                ]
            );
        }
    }

    #[test]
    fn test_add_sparse_chunk_raw() {
        // Test they merge.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Raw { start: 0, size: 12288 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Raw { start: 0, size: 16384 }).unwrap();
            assert_eq!(1, res.len());
            assert_eq!(Chunk::Raw { start: 0, size: 28672 }, res[0]);
        }

        // Test they don't merge on different type.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Raw { start: 0, size: 12288 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 3, size: 8192, value: 1 }).unwrap();
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::Raw { start: 0, size: 12288 },
                    Chunk::Fill { start: 3, size: 8192, value: 1 }
                ]
            );
        }

        // Test they don't merge when too large.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Raw { start: 0, size: 4096 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Raw { start: 0, size: u32::MAX - 4095 }).unwrap();
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [
                    Chunk::Raw { start: 0, size: 4096 },
                    Chunk::Raw { start: 0, size: u32::MAX - 4095 }
                ]
            );
        }
    }

    #[test]
    fn test_add_sparse_chunk_crc32() {
        // Test they don't merge on same type (Crc32 is special).
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Crc32 { checksum: 1234 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Crc32 { checksum: 2345 }).unwrap();
            assert_eq!(2, res.len());
            assert_eq!(res, [Chunk::Crc32 { checksum: 1234 }, Chunk::Crc32 { checksum: 2345 }]);
        }

        // Test they don't merge on different type.
        {
            let mut init_vec = Vec::<Chunk>::new();
            init_vec.push(Chunk::Crc32 { checksum: 1234 });
            let mut res = init_vec.clone();
            add_sparse_chunk(&mut res, Chunk::Fill { start: 0, size: 4096, value: 1 }).unwrap();
            assert_eq!(2, res.len());
            assert_eq!(
                res,
                [Chunk::Crc32 { checksum: 1234 }, Chunk::Fill { start: 0, size: 4096, value: 1 }]
            );
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    // Integration
    //

    #[test]
    fn test_roundtrip() {
        let tmpdir = TempDir::new().unwrap();

        // Generate a large temporary file
        let (mut file, _temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng = SmallRng::from_entropy();
        let mut buf = Vec::<u8>::new();
        buf.resize(1 * 4096, 0);
        rng.fill_bytes(&mut buf);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let content_size = buf.len();

        // build a sparse file
        let mut sparse_file = NamedTempFile::new_in(&tmpdir).unwrap().into_file();
        SparseImageBuilder::new()
            .add_chunk(DataSource::Buffer(Box::new([0xffu8; 8192])))
            .add_chunk(DataSource::Reader { reader: Box::new(file), size: content_size as u64 })
            .add_chunk(DataSource::Fill(0xaaaa_aaaau32, 1024))
            .add_chunk(DataSource::Skip(16384))
            .build(&mut sparse_file)
            .expect("Build sparse image failed");
        sparse_file.seek(SeekFrom::Start(0)).unwrap();

        let mut orig_file = NamedTempFile::new_in(&tmpdir).unwrap().into_file();
        unsparse(&mut sparse_file, &mut orig_file).expect("unsparse failed");
        orig_file.seek(SeekFrom::Start(0)).unwrap();

        let mut unsparsed_bytes = vec![];
        orig_file.read_to_end(&mut unsparsed_bytes).expect("Failed to read unsparsed image");
        assert_eq!(unsparsed_bytes.len(), 8192 + 20480 + content_size);
        assert_eq!(&unsparsed_bytes[..8192], &[0xffu8; 8192]);
        assert_eq!(&unsparsed_bytes[8192..8192 + content_size], &buf[..]);
        assert_eq!(&unsparsed_bytes[8192 + content_size..12288 + content_size], &[0xaau8; 4096]);
        assert_eq!(&unsparsed_bytes[12288 + content_size..], &[0u8; 16384]);
    }

    #[test]
    /// test_with_simg2img is a "round trip" test that does the following
    ///
    /// 1. Generates a pseudorandom temporary file
    /// 2. Builds sparse files out of it
    /// 3. Uses the android tool simg2img to take the sparse files and generate
    ///    the "original" image file out of them.
    /// 4. Asserts the originally created file and the one created by simg2img
    ///    have binary equivalent contents.
    ///
    /// This gives us a reasonable expectation of correctness given that the
    /// Android-provided sparse tools are able to interpret our sparse images.
    #[cfg(target_os = "linux")]
    fn test_with_simg2img() {
        let simg2img_path = Path::new("./host_x64/test_data/storage/sparse/simg2img");
        assert!(
            Path::exists(simg2img_path),
            "simg2img binary must exist at {}",
            simg2img_path.display()
        );

        let tmpdir = TempDir::new().unwrap();

        // Generate a large temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng = SmallRng::from_entropy();
        let mut buf = Vec::<u8>::new();
        // Dont want it to neatly fit a block size
        buf.resize(50 * 4096 + 1244, 0);
        rng.fill_bytes(&mut buf);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        // build a sparse file
        let files = build_sparse_files(
            "test",
            temp_path.to_path_buf().to_str().expect("Should succeed"),
            tmpdir.path(),
            4096 * 2,
        )
        .unwrap();

        let mut simg2img_output = tmpdir.path().to_path_buf();
        simg2img_output.push("output");

        let mut simg2img = Command::new(simg2img_path)
            .args(&files[..])
            .arg(&simg2img_output)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn simg2img");
        let res = simg2img.wait().expect("simg2img did was not running");
        assert!(res.success(), "simg2img did not succeed");
        let mut simg2img_stdout = simg2img.stdout.take().expect("Get stdout from simg2img");
        let mut simg2img_stderr = simg2img.stderr.take().expect("Get stderr from simg2img");

        let mut stdout = String::new();
        simg2img_stdout.read_to_string(&mut stdout).expect("Reading simg2img stdout");
        assert_eq!(stdout, "");

        let mut stderr = String::new();
        simg2img_stderr.read_to_string(&mut stderr).expect("Reading simg2img stderr");
        assert_eq!(stderr, "");

        let simg2img_output_bytes =
            std::fs::read(simg2img_output).expect("Failed to read simg2img output");

        assert_eq!(
            buf,
            simg2img_output_bytes[0..buf.len()],
            "Output from simg2img should match our generated file"
        );

        assert_eq!(
            simg2img_output_bytes[buf.len()..],
            vec![0u8; simg2img_output_bytes.len() - buf.len()],
            "The remainder of our simg2img_output_bytes should be 0"
        );
    }
}
