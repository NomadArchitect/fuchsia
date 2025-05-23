// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use assert_matches::assert_matches;
use fidl_fuchsia_io as fio;
use futures::stream::StreamExt as _;
use std::io::Write as _;
use std::time::Duration;
use zx::{AsHandleRef as _, Status};

// merkle root of b"Hello world!\n".
static BLOB_MERKLE: &str = "e5892a9b652ede2e19460a9103fd9cb3417f782a8d29f6c93ec0c31170a94af3";
static BLOB_CONTENTS: &[u8] = b"Hello world!\n";

fn ls_simple(d: openat::DirIter) -> Result<Vec<String>, Error> {
    Ok(d.map(|i| i.map(|entry| entry.file_name().to_string_lossy().into()))
        .collect::<Result<Vec<_>, _>>()?)
}

#[fuchsia_async::run_singlethreaded(test)]
async fn blobfs() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;

    let d = blobfs_server.root_dir().context("get root dir")?;
    assert_eq!(
        ls_simple(d.list_dir(".").expect("list dir")).expect("list dir contents"),
        Vec::<String>::new(),
    );

    let mut f = d.write_file(BLOB_MERKLE, 0).expect("open file 1");
    f.set_len(6_u64).expect("truncate");

    f.write_all(b"Hello").unwrap_or_else(|e| eprintln!("write 1 error: {e}"));
    drop(f);

    assert_eq!(
        ls_simple(d.list_dir(".").expect("list dir")).expect("list dir contents"),
        Vec::<String>::new(),
    );

    let mut f = d.write_file(BLOB_MERKLE, 0).expect("open file 2");
    f.set_len(BLOB_CONTENTS.len() as u64).expect("truncate");
    f.write_all(b"Hello ").expect("write file2.1");
    f.write_all(b"world!\n").expect("write file2.2");
    drop(f);

    assert_eq!(
        ls_simple(d.list_dir(".").expect("list dir")).expect("list dir contents"),
        vec![BLOB_MERKLE.to_string()],
    );
    assert_eq!(
        blobfs_server.list_blobs().expect("list blobs"),
        BTreeSet::from([BLOB_MERKLE.parse().unwrap()]),
    );

    blobfs_server.stop().await?;

    Ok(())
}

async fn open_blob(
    blobfs: &fio::DirectoryProxy,
    merkle: &str,
    mut flags: fio::Flags,
) -> Result<(fio::FileProxy, zx::Event), zx::Status> {
    let (file, server_end) = fidl::endpoints::create_proxy::<fio::FileMarker>();

    flags |= fio::Flags::FLAG_SEND_REPRESENTATION;
    blobfs.open(merkle, flags, &Default::default(), server_end.into_channel()).expect("open blob");

    let event = file
        .take_event_stream()
        .next()
        .await
        .expect("fio::FileEvent stream must be non empty!")
        .map_err(|e| match e {
            fidl::Error::ClientChannelClosed { status, .. } => status,
            _ => panic!("fio::FileEvent stream contained fidl error"),
        })?;
    let observer = match event {
        fio::FileEvent::OnOpen_ { .. } => {
            panic!("Expected OnRepresentation event, got OnOpen instead!");
        }
        fio::FileEvent::OnRepresentation { payload } => match payload {
            fio::Representation::File(fio::FileInfo { observer: Some(observer), .. }) => observer,
            other => panic!(
                "ConnectionInfo from fio::FileEventStream to be File variant with event: {other:?}"
            ),
        },
        fio::FileEvent::_UnknownEvent { ordinal, .. } => panic!("Unknown file event {ordinal}"),
    };
    Ok((file, observer))
}

async fn write_blob(blob: &fio::FileProxy, bytes: &[u8]) -> Result<(), Error> {
    let n = blob.write(bytes).await?.map_err(zx::Status::from_raw)?;
    assert_eq!(n, bytes.len() as u64);
    Ok(())
}

/// Verify the contents of a blob, or return any non-ok zx status encountered along the way.
async fn verify_blob(blob: &fio::FileProxy, expected_bytes: &[u8]) -> Result<(), Status> {
    let actual_bytes = blob
        .read_at(expected_bytes.len() as u64 + 1, 0)
        .await
        .unwrap()
        .map_err(Status::from_raw)?;
    assert_eq!(actual_bytes, expected_bytes);
    Ok(())
}

async fn create_blob(
    blobfs: &fio::DirectoryProxy,
    merkle: &str,
    contents: &[u8],
) -> Result<(), Error> {
    let (blob, _) = open_blob(
        blobfs,
        merkle,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(contents.len() as u64).await?.map_err(Status::from_raw)?;
    write_blob(&blob, contents).await?;
    blob.close().await?.map_err(Status::from_raw)?;

    let (blob, _) = open_blob(blobfs, merkle, fio::PERM_READABLE).await?;
    verify_blob(&blob, contents).await?;
    Ok(())
}

// Dropping a FileProxy synchronously closes the zircon channel, but it is not guaranteed
// that blobfs will respond to the channel closing before it responds to a request on a
// separate channel to open the same blob. This means a test case that:
// 1. opens writable + resizes on channel 0
// 2. drops channel 0
// 3. opens writable on channel 1
// can fail with ACCESS_DENIED in step 3, unless we wait.
async fn wait_for_blob_to_be_creatable(blobfs: &fio::DirectoryProxy, merkle: &str) {
    for _ in 0..50 {
        let res = open_blob(
            blobfs,
            merkle,
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
        )
        .await;
        match res {
            Err(zx::Status::ACCESS_DENIED) => {
                fuchsia_async::Timer::new(Duration::from_millis(10)).await;
                continue;
            }
            Err(err) => {
                panic!("unexpected error waiting for blob to become writable: {err:?}");
            }
            Ok((blob, _)) => {
                // Explicitly close the blob so that when this function returns the blob
                // is in the state (creatable + not openable for read). If we just drop
                // the FileProxy instead of closing, the blob will be openable for read until
                // blobfs asynchronously cleans up.
                blob.close().await.unwrap().map_err(Status::from_raw).unwrap();
                return;
            }
        }
    }
    panic!("timeout waiting for blob to become creatable");
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_for_create_create() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (_blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;

    create_blob(&root_dir, BLOB_MERKLE, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_resize_drop_create() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    drop(blob);
    wait_for_blob_to_be_creatable(&root_dir, BLOB_MERKLE).await;

    create_blob(&root_dir, BLOB_MERKLE, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_partial_write_drop_create() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    write_blob(&blob, &BLOB_CONTENTS[0..1]).await?;
    drop(blob);
    wait_for_blob_to_be_creatable(&root_dir, BLOB_MERKLE).await;

    create_blob(&root_dir, BLOB_MERKLE, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_partial_write_close_create() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    write_blob(&blob, &BLOB_CONTENTS[0..1]).await?;
    blob.close().await?.map_err(Status::from_raw)?;

    create_blob(&root_dir, BLOB_MERKLE, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_resize_open_for_create_fails() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;

    let res = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await;

    assert_matches!(res, Err(zx::Status::ACCESS_DENIED));

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_open_resize_resize_fails() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let (blob1, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    let result = blob1.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw);
    assert_matches!(result, Err(zx::Status::BAD_STATE));

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open0_open1_resize1_write1_succeeds() {
    let blobfs_server = BlobfsRamdisk::start().await.unwrap();
    let root_dir = blobfs_server.root_dir_proxy().unwrap();

    let (_blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();

    let (blob1, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    let () = blob1.resize(BLOB_CONTENTS.len() as u64).await.unwrap().unwrap();
    let () = write_blob(&blob1, BLOB_CONTENTS).await.unwrap();

    let (blob2, _) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await.unwrap();
    let () = verify_blob(&blob2, BLOB_CONTENTS).await.unwrap();

    let () = blobfs_server.stop().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open0_open1_resize0_write0_succeeds() {
    let blobfs_server = BlobfsRamdisk::start().await.unwrap();
    let root_dir = blobfs_server.root_dir_proxy().unwrap();

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();

    let (_blob1, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();

    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await.unwrap().unwrap();
    let () = write_blob(&blob0, BLOB_CONTENTS).await.unwrap();

    let (blob2, _) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await.unwrap();
    let () = verify_blob(&blob2, BLOB_CONTENTS).await.unwrap();

    let () = blobfs_server.stop().await.unwrap();
}

/// On c++blobfs, open connections keep even partially written blobs alive.
/// Testing for blob presence by opening a blob can therefore conflict with a concurrent operation
/// that is writing a blob, as the partial blob from a failed write attempt can be kept alive by the
/// connection from the presence test and prevent the writer from recreating the blob for a retry
/// attempt.
#[fuchsia_async::run_singlethreaded(test)]
async fn open0_open1_resize0_close0_open2_fails() {
    let blobfs_server = BlobfsRamdisk::start().await.unwrap();
    let root_dir = blobfs_server.root_dir_proxy().unwrap();

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();

    let (blob1, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();

    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await.unwrap().unwrap();

    let () = blob0.close().await.unwrap().unwrap();

    // The outstanding blob1 write connection keeps the partially written blob alive, preventing new
    // write attempts.
    assert_matches!(
        open_blob(
            &root_dir,
            BLOB_MERKLE,
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE
        )
        .await,
        Err(zx::Status::ACCESS_DENIED)
    );

    // Closing the outstanding connection allows the blob to be created.
    let () = blob1.close().await.unwrap().unwrap();

    let (blob2, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    let () = blob2.resize(BLOB_CONTENTS.len() as u64).await.unwrap().unwrap();
    let () = write_blob(&blob2, BLOB_CONTENTS).await.unwrap();

    let (blob3, _) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await.unwrap();
    let () = verify_blob(&blob3, BLOB_CONTENTS).await.unwrap();

    let () = blobfs_server.stop().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_resize_open_read_fails() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    let (blob1, _) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await?;

    let result = blob1.read_at(1, 0).await?.map_err(zx::Status::from_raw);

    assert_eq!(result, Err(zx::Status::BAD_STATE));

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_for_create_wait_for_signal() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let (blob1, event) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await?;
    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    assert_matches!(
        event
            .wait_handle(
                zx::Signals::all(),
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(0))
            )
            .to_result(),
        Err(zx::Status::TIMED_OUT)
    );
    write_blob(&blob0, BLOB_CONTENTS).await?;

    assert_eq!(
        event
            .wait_handle(
                zx::Signals::all(),
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(0))
            )
            .to_result()?,
        zx::Signals::USER_0
    );
    verify_blob(&blob1, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_resize_wait_for_signal() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;
    let (blob1, event) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await?;
    assert_matches!(
        event
            .wait_handle(
                zx::Signals::all(),
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(0))
            )
            .to_result(),
        Err(zx::Status::TIMED_OUT)
    );
    write_blob(&blob0, BLOB_CONTENTS).await?;

    assert_eq!(
        event
            .wait_handle(
                zx::Signals::all(),
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(0))
            )
            .to_result()?,
        zx::Signals::USER_0
    );
    verify_blob(&blob1, BLOB_CONTENTS).await?;

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn empty_blob_readable_after_resize() {
    let empty_hash = fuchsia_merkle::from_slice(&[][..]).root().to_string();

    let blobfs_server = BlobfsRamdisk::start().await.unwrap();
    let root_dir = blobfs_server.root_dir_proxy().unwrap();

    let (blob0, _) = open_blob(
        &root_dir,
        &empty_hash,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    let () = blob0.resize(0).await.unwrap().map_err(Status::from_raw).unwrap();

    let (blob1, event) = open_blob(&root_dir, &empty_hash, fio::PERM_READABLE).await.unwrap();
    assert_matches!(
        event
            .wait_handle(
                zx::Signals::all(),
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(0))
            )
            .to_result(),
        Ok(zx::Signals::USER_0)
    );
    verify_blob(&blob1, &[]).await.unwrap();

    blobfs_server.stop().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn open_missing_fails() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let res = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await;

    assert_matches!(res, Err(zx::Status::NOT_FOUND));

    blobfs_server.stop().await
}

// ReadDirents on /blob should only return blobs if they are fully written and do not have
// outstanding deletion requests.
#[fuchsia_async::run_singlethreaded(test)]
async fn readdirents_only_returns_valid_blobs() {
    let blobfs_server = BlobfsRamdisk::start().await.unwrap();
    let root_dir = blobfs_server.root_dir_proxy().unwrap();

    // Blob doesn't appear until it is fully written.
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let (blob0, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = blob0.resize(BLOB_CONTENTS.len() as u64).await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = write_blob(&blob0, &BLOB_CONTENTS[0..BLOB_CONTENTS.len() - 1]).await.unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = write_blob(&blob0, &BLOB_CONTENTS[BLOB_CONTENTS.len() - 1..]).await.unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::from([BLOB_MERKLE.parse().unwrap()]));

    // Blob disappears once a deletion request has been received, even if an outstanding connection
    // is keeping it alive.
    let (blob1, _) = open_blob(&root_dir, BLOB_MERKLE, fio::PERM_READABLE).await.unwrap();

    let client = blobfs_server.client();
    let () = client.delete_blob(&BLOB_MERKLE.parse().unwrap()).await.unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = blob0.close().await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = blob1.close().await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = blobfs_server.stop().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn corrupt_create_fails_on_last_byte_write() -> Result<(), Error> {
    let blobfs_server = BlobfsRamdisk::start().await?;
    let root_dir = blobfs_server.root_dir_proxy()?;

    let (blob, _) = open_blob(
        &root_dir,
        BLOB_MERKLE,
        fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE | fio::PERM_WRITABLE,
    )
    .await?;
    let () = blob.resize(BLOB_CONTENTS.len() as u64).await?.map_err(Status::from_raw)?;

    write_blob(&blob, &BLOB_CONTENTS[..BLOB_CONTENTS.len() - 1]).await?;
    let wrong_trailing_byte = !BLOB_CONTENTS.last().unwrap();
    assert_matches!(
        write_blob(&blob, &[wrong_trailing_byte]).await,
        Err(e) if *e.downcast_ref::<zx::Status>().unwrap() == zx::Status::IO_DATA_INTEGRITY
    );

    blobfs_server.stop().await
}

#[fuchsia_async::run_singlethreaded(test)]
async fn fxblob_concurrent_creation_succeeds() {
    let blobfs = BlobfsRamdisk::builder().fxblob().start().await.unwrap();
    let creator = blobfs.blob_creator_proxy().unwrap().unwrap();

    // 8,194 bytes so that the partial write exceeds 8,192 bytes.
    let bytes = vec![0u8; 8194];
    let hash = fuchsia_merkle::from_slice(&bytes).root();
    let compressed = Type1Blob::generate(&bytes, CompressionMode::Never);
    let compressed_len: u64 = compressed.len().try_into().unwrap();

    let writer0 = creator.create(&hash, false).await.unwrap().unwrap().into_proxy();
    let vmo0 = writer0.get_vmo(compressed_len).await.unwrap().unwrap();
    let () = vmo0.write(&compressed, 0).unwrap();
    let () = writer0.bytes_ready(compressed_len - 1).await.unwrap().unwrap();
    assert_eq!(blobfs.list_blobs().unwrap(), BTreeSet::new());

    let writer1 = creator.create(&hash, false).await.unwrap().unwrap().into_proxy();
    let vmo1 = writer1.get_vmo(compressed_len).await.unwrap().unwrap();
    let () = vmo1.write(&compressed, 0).unwrap();
    let () = writer1.bytes_ready(compressed_len).await.unwrap().unwrap();
    assert_eq!(blobfs.list_blobs().unwrap(), BTreeSet::from([hash]));

    blobfs.stop().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn fxblob_create_already_present_returns_already_exists() {
    let blobfs = BlobfsRamdisk::builder().fxblob().start().await.unwrap();
    let creator = blobfs.blob_creator_proxy().unwrap().unwrap();

    let bytes = vec![0u8; 1];
    let hash = fuchsia_merkle::from_slice(&bytes).root();
    let compressed = Type1Blob::generate(&bytes, CompressionMode::Never);
    let compressed_len: u64 = compressed.len().try_into().unwrap();

    let writer0 = creator.create(&hash, false).await.unwrap().unwrap().into_proxy();
    let vmo0 = writer0.get_vmo(compressed_len).await.unwrap().unwrap();
    let () = vmo0.write(&compressed, 0).unwrap();
    let () = writer0.bytes_ready(compressed_len).await.unwrap().unwrap();
    assert_eq!(blobfs.list_blobs().unwrap(), BTreeSet::from([hash]));

    assert_matches!(
        creator.create(&hash, false).await,
        Ok(Err(ffxfs::CreateBlobError::AlreadyExists))
    );

    blobfs.stop().await.unwrap();
}

// ReadDirents on /blob should only return blobs if they are fully written and do not have
// outstanding deletion requests.
#[fuchsia_async::run_singlethreaded(test)]
async fn fxblob_readdirents_only_returns_valid_blobs() {
    let blobfs_server = BlobfsRamdisk::builder().fxblob().start().await.unwrap();
    let creator = blobfs_server.blob_creator_proxy().unwrap().unwrap();
    let bytes = vec![0u8; 1];
    let hash = fuchsia_merkle::from_slice(&bytes).root();
    let compressed = Type1Blob::generate(&bytes, CompressionMode::Never);
    let compressed_len: u64 = compressed.len().try_into().unwrap();

    // Blob doesn't appear until it is fully written.
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let writer0 = creator.create(&hash, false).await.unwrap().unwrap().into_proxy();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let vmo0 = writer0.get_vmo(compressed_len).await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = vmo0.write(&compressed, 0).unwrap();
    let () = writer0.bytes_ready(compressed_len - 1).await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = writer0.bytes_ready(1).await.unwrap().unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::from([hash]));

    // Blob disappears once a deletion request has been received, even if an outstanding connection
    // is keeping it alive.
    let reader = blobfs_server.blob_reader_proxy().unwrap().unwrap();
    let _vmo1: zx::Vmo = reader.get_vmo(&hash.into()).await.unwrap().unwrap();

    let () = blobfs_server.client().delete_blob(&hash).await.unwrap();
    assert_eq!(blobfs_server.list_blobs().unwrap(), BTreeSet::new());

    let () = blobfs_server.stop().await.unwrap();
}
