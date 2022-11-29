// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    device_watcher::recursive_wait_and_open_node,
    fidl::endpoints::{create_proxy, Proxy},
    fidl_fuchsia_fxfs::{CryptManagementMarker, CryptMarker, KeyPurpose},
    fidl_fuchsia_io as fio, fidl_fuchsia_logger as flogger,
    fs_management::{Blobfs, Fxfs, BLOBFS_TYPE_GUID, DATA_TYPE_GUID},
    fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route},
    fuchsia_zircon::{self as zx, HandleBased},
    key_bag::Aes256Key,
    ramdevice_client::VmoRamdiskClientBuilder,
    std::{io::Write, ops::Deref, path::Path},
    storage_isolated_driver_manager::{
        fvm::{create_fvm_volume, set_up_fvm},
        zxcrypt,
    },
    uuid::Uuid,
    zerocopy::AsBytes,
};

// We set the default disk size to be twice the value of
// DEFAULT_F2FS_MIN_BYTES (defined in device/constants.rs)
// to ensure that when f2fs is the data filesystem format,
// we don't run out of space. Similarly, the size of the data
// volume should be > DEFAULT_F2FS_MIN_BYTES but < disk size
pub const DEFAULT_DISK_SIZE: u64 = 200 * 1024 * 1024;
pub const DEFAULT_DATA_VOLUME_SIZE: u64 = 101 * 1024 * 1024;

// We use a static key-bag so that the crypt instance can be shared across test executions safely.
// These keys match the DATA_KEY and METADATA_KEY respectively, when wrapped with the "zxcrypt"
// static key used by fshost.
// Note this isn't used in the legacy crypto format.
const KEY_BAG_CONTENTS: &'static str = "\
{
    \"version\":1,
    \"keys\": {
        \"0\":{
            \"Aes128GcmSivWrapped\": [
                \"7a7c6a718cfde7078f6edec5\",
                \"7cc31b765c74db3191e269d2666267022639e758fe3370e8f36c166d888586454fd4de8aeb47aadd81c531b0a0a66f27\"
            ]
        },
        \"1\":{
            \"Aes128GcmSivWrapped\": [
                \"b7d7f459cbee4cc536cc4324\",
                \"9f6a5d894f526b61c5c091e5e02a7ff94d18e6ad36a0aa439c86081b726eca79e6b60bd86ee5d86a20b3df98f5265a99\"
            ]
        }
    }
}";

const DATA_KEY: Aes256Key = Aes256Key::create([
    0xcf, 0x9e, 0x45, 0x2a, 0x22, 0xa5, 0x70, 0x31, 0x33, 0x3b, 0x4d, 0x6b, 0x6f, 0x78, 0x58, 0x29,
    0x04, 0x79, 0xc7, 0xd6, 0xa9, 0x4b, 0xce, 0x82, 0x04, 0x56, 0x5e, 0x82, 0xfc, 0xe7, 0x37, 0xa8,
]);

const METADATA_KEY: Aes256Key = Aes256Key::create([
    0x0f, 0x4d, 0xca, 0x6b, 0x35, 0x0e, 0x85, 0x6a, 0xb3, 0x8c, 0xdd, 0xe9, 0xda, 0x0e, 0xc8, 0x22,
    0x8e, 0xea, 0xd8, 0x05, 0xc4, 0xc9, 0x0b, 0xa8, 0xd8, 0x85, 0x87, 0x50, 0x75, 0x40, 0x1c, 0x4c,
]);

// Matches the hard-coded value used by fshost when use_native_fxfs_crypto is false.
const LEGACY_DATA_KEY: Aes256Key = Aes256Key::create([
    0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf, 0x10, 0x11,
    0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
]);

// Matches the hard-coded value used by fshost when use_native_fxfs_crypto is false.
const LEGACY_METADATA_KEY: Aes256Key = Aes256Key::create([
    0xff, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa, 0xf9, 0xf8, 0xf7, 0xf6, 0xf5, 0xf4, 0xf3, 0xf2, 0xf1, 0xf0,
    0xef, 0xee, 0xed, 0xec, 0xeb, 0xea, 0xe9, 0xe8, 0xe7, 0xe6, 0xe5, 0xe4, 0xe3, 0xe2, 0xe1, 0xe0,
]);

async fn create_hermetic_crypt_service(
    data_key: Aes256Key,
    metadata_key: Aes256Key,
) -> RealmInstance {
    let builder = RealmBuilder::new().await.unwrap();
    let url = "#meta/fxfs-crypt.cm";
    let crypt = builder.add_child("fxfs-crypt", url, ChildOptions::new().eager()).await.unwrap();
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<CryptMarker>())
                .capability(Capability::protocol::<CryptManagementMarker>())
                .from(&crypt)
                .to(Ref::parent()),
        )
        .await
        .unwrap();
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<flogger::LogSinkMarker>())
                .from(Ref::parent())
                .to(&crypt),
        )
        .await
        .unwrap();
    let realm = builder.build().await.expect("realm build failed");
    let crypt_management =
        realm.root.connect_to_protocol_at_exposed_dir::<CryptManagementMarker>().unwrap();
    crypt_management
        .add_wrapping_key(0, data_key.deref())
        .await
        .unwrap()
        .expect("add_wrapping_key failed");
    crypt_management
        .add_wrapping_key(1, metadata_key.deref())
        .await
        .unwrap()
        .expect("add_wrapping_key failed");
    crypt_management
        .set_active_key(KeyPurpose::Data, 0)
        .await
        .unwrap()
        .expect("set_active_key failed");
    crypt_management
        .set_active_key(KeyPurpose::Metadata, 1)
        .await
        .unwrap()
        .expect("set_active_key failed");
    realm
}

pub enum Disk {
    Prebuilt(zx::Vmo),
    Builder(DiskBuilder),
}

impl Disk {
    pub async fn get_vmo(self) -> zx::Vmo {
        match self {
            Disk::Prebuilt(vmo) => vmo,
            Disk::Builder(builder) => builder.build().await,
        }
    }

    pub fn builder(&mut self) -> &mut DiskBuilder {
        match self {
            Disk::Prebuilt(_) => panic!("attempted to get builder for prebuilt disk"),
            Disk::Builder(builder) => builder,
        }
    }
}

pub struct DiskBuilder {
    size: u64,
    data_volume_size: u64,
    format: Option<&'static str>,
    legacy_crypto_format: bool,
    zxcrypt: bool,
}

impl DiskBuilder {
    pub fn new() -> DiskBuilder {
        DiskBuilder {
            size: DEFAULT_DISK_SIZE,
            data_volume_size: DEFAULT_DATA_VOLUME_SIZE,
            format: None,
            legacy_crypto_format: false,
            zxcrypt: true,
        }
    }

    pub fn size(&mut self, size: u64) -> &mut Self {
        self.size = size;
        self
    }

    pub fn data_volume_size(&mut self, data_volume_size: u64) -> &mut Self {
        self.data_volume_size = data_volume_size;
        self
    }

    pub fn format_data(&mut self, format: &'static str) -> &mut Self {
        self.format = Some(format);
        self
    }

    pub fn legacy_crypto_format(&mut self) -> &mut Self {
        self.legacy_crypto_format = true;
        self
    }

    pub fn without_zxcrypt(&mut self) -> &mut Self {
        self.zxcrypt = false;
        self
    }

    pub(crate) async fn build(self) -> zx::Vmo {
        let (dev, server) = create_proxy::<fio::DirectoryMarker>().unwrap();
        fdio::open(
            "/dev",
            fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
            server.into_channel(),
        )
        .unwrap();

        recursive_wait_and_open_node(&dev, "sys/platform/00:00:2d/ramctl")
            .await
            .expect("recursive_wait_and_open_node failed");

        let vmo = zx::Vmo::create(self.size).unwrap();
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let ramdisk = VmoRamdiskClientBuilder::new(vmo).block_size(512).build().unwrap();
        let ramdisk_path = ramdisk.get_path();

        let volume_manager_proxy =
            set_up_fvm(Path::new(ramdisk_path), 32 * 1024).await.expect("set_up_fvm failed");

        create_fvm_volume(
            &volume_manager_proxy,
            "blobfs",
            &BLOBFS_TYPE_GUID,
            Uuid::new_v4().as_bytes(),
            None,
            0,
        )
        .await
        .expect("create_fvm_volume failed");
        let blobfs_path = format!("{}/fvm/blobfs-p-1/block", ramdisk_path);
        recursive_wait_and_open_node(&dev, &blobfs_path.strip_prefix("/dev/").unwrap())
            .await
            .expect("recursive_wait_and_open_node failed");
        let mut blobfs = Blobfs::new(&blobfs_path).expect("new failed");
        blobfs.format().await.expect("format failed");
        create_fvm_volume(
            &volume_manager_proxy,
            "data",
            &DATA_TYPE_GUID,
            Uuid::new_v4().as_bytes(),
            Some(self.data_volume_size),
            0,
        )
        .await
        .expect("create_fvm_volume failed");

        if let Some(format) = self.format {
            match format {
                "fxfs" => self.init_data_fxfs(ramdisk_path, &dev).await,
                "minfs" => self.init_data_minfs(ramdisk_path, &dev).await,
                "f2fs" => self.init_data_f2fs(ramdisk_path, &dev).await,
                _ => panic!("unsupported data filesystem format type"),
            }
        }

        ramdisk.destroy().expect("destroy failed");

        vmo_dup
    }

    async fn init_data_minfs(&self, ramdisk_path: &str, dev: &fio::DirectoryProxy) {
        let data_path = format!("{}/fvm/data-p-2/block", ramdisk_path);
        let mut data_device =
            recursive_wait_and_open_node(&dev, &data_path.strip_prefix("/dev/").unwrap())
                .await
                .expect("recursive_wait_and_open_node failed");
        if self.zxcrypt {
            let zxcrypt_path = zxcrypt::set_up_insecure_zxcrypt(Path::new(&data_path))
                .await
                .expect("failed to set up zxcrypt");
            let zxcrypt_path = zxcrypt_path.as_os_str().to_str().unwrap();
            data_device =
                recursive_wait_and_open_node(dev, zxcrypt_path.strip_prefix("/dev/").unwrap())
                    .await
                    .expect("recursive_wait_and_open_node failed");
        }
        let mut minfs = fs_management::Minfs::from_channel(
            data_device.into_channel().unwrap().into_zx_channel(),
        )
        .expect("from_channel failed");
        minfs.format().await.expect("format failed");
        let fs = minfs.serve().await.expect("serve_single_volume failed");
        // Create a file called "foo" that tests can test for presence.
        let (file, server) = create_proxy::<fio::NodeMarker>().unwrap();
        fs.root()
            .open(
                fio::OpenFlags::RIGHT_READABLE
                    | fio::OpenFlags::RIGHT_WRITABLE
                    | fio::OpenFlags::CREATE,
                0,
                "foo",
                server,
            )
            .expect("open failed");
        // We must solicit a response since otherwise shutdown below could race and creation of
        // the file could get dropped.
        let _: Vec<_> = file.query().await.expect("query failed");
        fs.shutdown().await.expect("shutdown failed");
    }

    async fn init_data_f2fs(&self, ramdisk_path: &str, dev: &fio::DirectoryProxy) {
        let data_path = format!("{}/fvm/data-p-2/block", ramdisk_path);
        let mut data_device =
            recursive_wait_and_open_node(&dev, &data_path.strip_prefix("/dev/").unwrap())
                .await
                .expect("recursive_wait_and_open_node failed");
        if self.zxcrypt {
            let zxcrypt_path = zxcrypt::set_up_insecure_zxcrypt(Path::new(&data_path))
                .await
                .expect("failed to set up zxcrypt");
            let zxcrypt_path = zxcrypt_path.as_os_str().to_str().unwrap();
            data_device =
                recursive_wait_and_open_node(dev, zxcrypt_path.strip_prefix("/dev/").unwrap())
                    .await
                    .expect("recursive_wait_and_open_node failed");
        }
        let mut f2fs = fs_management::F2fs::from_channel(
            data_device.into_channel().unwrap().into_zx_channel(),
        )
        .expect("from_channel failed");
        f2fs.format().await.expect("format failed");
        let fs = f2fs.serve().await.expect("serve_single_volume failed");
        // Create a file called "foo" that tests can test for presence.
        let (file, server) = create_proxy::<fio::NodeMarker>().unwrap();
        fs.root()
            .open(
                fio::OpenFlags::RIGHT_READABLE
                    | fio::OpenFlags::RIGHT_WRITABLE
                    | fio::OpenFlags::CREATE,
                0,
                "foo",
                server,
            )
            .expect("open failed");
        // We must solicit a response since otherwise shutdown below could race and creation of
        // the file could get dropped.
        let _: Vec<_> = file.query().await.expect("query failed");
        fs.shutdown().await.expect("shutdown failed");
    }

    async fn init_data_fxfs(&self, ramdisk_path: &str, dev: &fio::DirectoryProxy) {
        let (data_key, metadata_key) = if self.legacy_crypto_format {
            (LEGACY_DATA_KEY, LEGACY_METADATA_KEY)
        } else {
            (DATA_KEY, METADATA_KEY)
        };
        let crypt_realm = create_hermetic_crypt_service(data_key, metadata_key).await;
        let data_path = format!("{}/fvm/data-p-2/block", ramdisk_path);
        let data_device =
            recursive_wait_and_open_node(dev, data_path.strip_prefix("/dev/").unwrap())
                .await
                .expect("recursive_wait_and_open_node failed");
        let mut fxfs = Fxfs::from_channel(data_device.into_channel().unwrap().into_zx_channel())
            .expect("from_channel failed");
        fxfs.format().await.expect("format failed");
        let mut fs = fxfs.serve_multi_volume().await.expect("serve_multi_volume failed");
        let vol = if self.legacy_crypto_format {
            let crypt_service = Some(
                crypt_realm
                    .root
                    .connect_to_protocol_at_exposed_dir::<CryptMarker>()
                    .expect("Unable to connect to Crypt service")
                    .into_channel()
                    .unwrap()
                    .into_zx_channel()
                    .into(),
            );
            fs.create_volume("default", crypt_service).await.expect("create_volume failed")
        } else {
            let vol = fs.create_volume("unencrypted", None).await.expect("create_volume failed");
            vol.bind_to_path("/unencrypted_volume").unwrap();
            // Initialize the key-bag with the static keys.
            std::fs::create_dir("/unencrypted_volume/keys").expect("create_dir failed");
            let mut file = std::fs::File::create("/unencrypted_volume/keys/fxfs-data")
                .expect("create file failed");
            file.write_all(KEY_BAG_CONTENTS.as_bytes()).expect("write file failed");

            let crypt_service = Some(
                crypt_realm
                    .root
                    .connect_to_protocol_at_exposed_dir::<CryptMarker>()
                    .expect("Unable to connect to Crypt service")
                    .into_channel()
                    .unwrap()
                    .into_zx_channel()
                    .into(),
            );
            fs.create_volume("data", crypt_service).await.expect("create_volume failed")
        };
        // Create a file called "foo" that tests can test for presence.
        let (file, server) = create_proxy::<fio::NodeMarker>().unwrap();
        vol.root()
            .open(
                fio::OpenFlags::RIGHT_READABLE
                    | fio::OpenFlags::RIGHT_WRITABLE
                    | fio::OpenFlags::CREATE,
                0,
                "foo",
                server,
            )
            .expect("open failed");
        // We must solicit a response since otherwise shutdown below could race and creation of
        // the file could get dropped.
        let _: Vec<_> = file.query().await.expect("query failed");
        fs.shutdown().await.expect("shutdown failed");
    }

    /// Create a vmo artifact with the format of a compressed zbi boot item containing this
    /// filesystem.
    pub(crate) async fn build_as_zbi_ramdisk(self) -> zx::Vmo {
        /// The following types and constants are defined in
        /// zircon/system/public/zircon/boot/image.h.
        const ZBI_TYPE_STORAGE_RAMDISK: u32 = 0x4b534452;
        const ZBI_FLAGS_VERSION: u32 = 0x00010000;
        const ZBI_ITEM_MAGIC: u32 = 0xb5781729;
        const ZBI_FLAGS_STORAGE_COMPRESSED: u32 = 0x00000001;

        #[repr(C)]
        #[derive(AsBytes)]
        struct ZbiHeader {
            type_: u32,
            length: u32,
            extra: u32,
            flags: u32,
            _reserved0: u32,
            _reserved1: u32,
            magic: u32,
            _crc32: u32,
        }

        let ramdisk_vmo = self.build().await;
        let extra = ramdisk_vmo.get_size().unwrap() as u32;
        let mut decompressed_buf = vec![0u8; extra as usize];
        ramdisk_vmo.read(&mut decompressed_buf, 0).unwrap();
        let compressed_buf = zstd::encode_all(decompressed_buf.as_slice(), 0).unwrap();
        let length = compressed_buf.len() as u32;

        let header = ZbiHeader {
            type_: ZBI_TYPE_STORAGE_RAMDISK,
            length,
            extra,
            flags: ZBI_FLAGS_VERSION | ZBI_FLAGS_STORAGE_COMPRESSED,
            _reserved0: 0,
            _reserved1: 0,
            magic: ZBI_ITEM_MAGIC,
            _crc32: 0,
        };

        let header_size = std::mem::size_of::<ZbiHeader>() as u64;
        let zbi_vmo = zx::Vmo::create(header_size + length as u64).unwrap();
        zbi_vmo.write(header.as_bytes(), 0).unwrap();
        zbi_vmo.write(&compressed_buf, header_size).unwrap();

        zbi_vmo
    }
}
