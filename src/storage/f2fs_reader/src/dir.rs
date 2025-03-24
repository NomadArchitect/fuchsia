// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::BLOCK_SIZE;
use anyhow::{anyhow, ensure, Context, Error};
use enumn::N;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Ref, Unaligned};

#[derive(Copy, Clone, Debug, Eq, PartialEq, N)]
#[repr(u8)]
pub enum FileType {
    Unknown = 0,
    RegularFile = 1,
    Directory = 2,
    CharDevice = 3,
    BlockDevice = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct RawDirEntry {
    pub hash_code: u32,
    pub ino: u32,
    pub name_len: u16,
    pub file_type: u8,
}

#[derive(Clone, Debug)]
pub struct InlineDentry {
    pub dentry_bitmap: Box<[u8]>,
    pub dentry: Box<[RawDirEntry]>,
    pub filenames: Box<[u8]>,
}

pub const NAME_LEN: usize = 8;
pub const NUM_DENTRY_IN_BLOCK: usize = 214;
/// One bit per entry rounded up to the next byte.
pub const SIZE_OF_DENTRY_BITMAP: usize = (NUM_DENTRY_IN_BLOCK + 7) / 8;
/// Reserve space ensures we fill the block.
pub const SIZE_OF_DENTRY_RESERVED: usize = BLOCK_SIZE
    - ((std::mem::size_of::<RawDirEntry>() + NAME_LEN) * NUM_DENTRY_IN_BLOCK
        + SIZE_OF_DENTRY_BITMAP);

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, FromBytes, Immutable, KnownLayout, IntoBytes, Unaligned)]
pub struct DentryBlock {
    dentry_bitmap: [u8; SIZE_OF_DENTRY_BITMAP],
    _reserved: [u8; SIZE_OF_DENTRY_RESERVED],
    dentry: [RawDirEntry; NUM_DENTRY_IN_BLOCK],
    filenames: [u8; NUM_DENTRY_IN_BLOCK * NAME_LEN],
}

#[derive(Debug)]
pub struct DirEntry {
    pub hash_code: u32,
    pub ino: u32,
    pub filename: String,
    pub file_type: FileType,
}

// Helper function for reading directory entries.
// Caller is required to ensure that these byte arrays are appropriately sized to avoid panics.
fn get_dir_entries(
    dentry_bitmap: &[u8],
    dentry: &[RawDirEntry],
    filenames: &[u8],
) -> Result<Vec<DirEntry>, Error> {
    debug_assert!(dentry_bitmap.len() * 8 >= dentry.len(), "bitmap too small");
    debug_assert_eq!(dentry.len() * 8, filenames.len(), "dentry len different to filenames len");
    let mut out = Vec::new();
    let mut i = 0;
    while i < dentry.len() {
        let entry = dentry[i];
        // The dentry bitmap marks all entries that should be read.
        if (dentry_bitmap[i / 8] >> (i % 8)) & 0x1 == 0 || dentry[i].ino == 0 {
            i += 1;
            continue;
        }
        let name_len = dentry[i].name_len as usize;
        if name_len == 0 {
            i += 1;
            continue;
        }
        // Filename slots are 8 bytes long but F2fs allows long filenames to span multiple slots.
        ensure!(i * NAME_LEN + name_len <= filenames.len(), "Filename doesn't fit in buffer");
        let raw_filename = &filenames[i * NAME_LEN..i * NAME_LEN + name_len];
        // Ignore dot files.
        if raw_filename == b"." || raw_filename == b".." {
            i += 1;
            continue;
        }
        // TODO(b/404680707): Do we need to consider handling devices with badly formed filenames?
        let filename = str::from_utf8(raw_filename).context("Bad UTF8 filename")?.to_string();
        let file_type = FileType::n(dentry[i].file_type).ok_or(anyhow!("Bad file type"))?;
        out.push(DirEntry { hash_code: entry.hash_code, ino: entry.ino, filename, file_type });
        i += (name_len + NAME_LEN - 1) / NAME_LEN;
    }
    Ok(out)
}

impl DentryBlock {
    pub fn get_entries(&self) -> Result<Vec<DirEntry>, Error> {
        get_dir_entries(&self.dentry_bitmap, &self.dentry, &self.filenames)
    }
}

impl crate::inode::Inode {
    pub fn get_inline_dir_entries(&self) -> Result<Option<Vec<DirEntry>>, Error> {
        if let Some(inline_dentry) = &self.inline_dentry {
            Ok(Some(get_dir_entries(
                &inline_dentry.dentry_bitmap,
                &inline_dentry.dentry,
                &inline_dentry.filenames,
            )?))
        } else {
            Ok(None)
        }
    }
}

impl InlineDentry {
    pub fn try_from_bytes(rest: &[u8]) -> Result<Self, Error> {
        ensure!(rest.len() % 4 == 0, "Bad alignment in inode inline_dentry");
        // inline data skips 4 additional bytes.
        let rest = &rest[4..];
        // The layout of an inline dentry block is:
        // +------------------+
        // | dentry_bitmap    | <-- N bits long, rounded up to next byte.
        // +------------------+
        // |    <padding>     |
        // +------------------+
        // | N x RawDirEntry  | <-- N * 11 bytes
        // +------------------+
        // | N x filenames    | <-- N * 8 bytes
        // +------------------+
        // Within the block all elements are byte-aligned.
        // Note that filenames and RawDirEntry are aligned to the end of the block whilst
        // dentry_bitmap and the RawDirEntry are aligned to the start.
        // (This is similar to the layout of DentryBlock.)
        //
        // There may be up to 19 bytes of padding between dentry_bitmap and RawDirEntry.
        // Nb: The following calculation is done in bits to account for the bitmap.
        let dentry_count =
            8 * rest.len() / (8 * (std::mem::size_of::<RawDirEntry>() + NAME_LEN) + 1);
        let (dentry_bitmap, rest): (Ref<_, [u8]>, _) =
            Ref::from_prefix_with_elems(rest, (dentry_count + 7) / 8).unwrap();
        let (rest, filenames): (_, Ref<_, [u8]>) =
            Ref::from_suffix_with_elems(rest, dentry_count * NAME_LEN).unwrap();
        // Nb: Alignment here is byte-aligned.
        let (_, dentry): (_, Ref<_, [RawDirEntry]>) =
            Ref::from_suffix_with_elems(rest, dentry_count).unwrap();
        Ok(InlineDentry {
            dentry_bitmap: (*dentry_bitmap).into(),
            dentry: (*dentry).into(),
            filenames: (*filenames).into(),
        })
    }
}
