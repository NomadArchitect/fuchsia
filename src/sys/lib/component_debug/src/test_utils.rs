// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use {
    crate::io::Directory,
    anyhow::Result,
    fidl::endpoints::{create_proxy_and_stream, ServerEnd},
    fidl_fuchsia_io as fio,
    fidl_fuchsia_io::DirectoryProxy,
    fidl_fuchsia_sys2 as fsys,
    fuchsia_async::Task,
    futures::StreamExt,
    std::collections::HashMap,
    std::fs::{create_dir, metadata, set_permissions, write},
    std::path::PathBuf,
    tempfile::tempdir,
};

#[derive(Clone)]
pub struct File {
    pub on_host: bool,
    pub name: &'static str,
    pub data: &'static str,
}

// Setup |RealmQuery| server with the given component instances.
pub fn serve_realm_query(
    mut instances: HashMap<String, (fsys::InstanceInfo, Option<Box<fsys::ResolvedState>>)>,
) -> fsys::RealmQueryProxy {
    let (client, mut stream) = create_proxy_and_stream::<fsys::RealmQueryMarker>().unwrap();
    Task::spawn(async move {
        loop {
            let (moniker, responder) = match stream.next().await.unwrap().unwrap() {
                fsys::RealmQueryRequest::GetInstanceInfo { moniker, responder } => {
                    (moniker, responder)
                }
                _ => panic!("Unexpected RealmQuery request"),
            };

            let response = instances.remove(&moniker);

            match response {
                Some(instance) => responder.send(&mut Ok(instance)).unwrap(),
                None => responder.send(&mut Err(fsys::RealmQueryError::InstanceNotFound)).unwrap(),
            };
        }
    })
    .detach();
    client
}

// Set the permissions of a path on host to read only.
pub fn set_path_to_read_only(path: PathBuf) -> Result<()> {
    match metadata(path.clone()) {
        Ok(metadata) => {
            let mut perm = metadata.permissions();
            perm.set_readonly(true);
            set_permissions(path, perm).unwrap();
            Ok(())
        }
        Err(e) => panic!("Could not set path to read only: {}", e),
    }
}

// Create an arbitrary path string with tmp as the root.
pub fn create_tmp_path() -> String {
    let tmp_dir = tempdir().unwrap();
    let tmp_path = String::from(tmp_dir.path().to_str().unwrap());
    return tmp_path;
}

// Create a new temporary directory to serve as the mock namespace.
pub fn serve_realm_query_with_namespace(
    server: ServerEnd<fio::DirectoryMarker>,
    seed_files: Vec<File>,
    is_read_only: bool,
) -> Result<()> {
    let tmp_path = create_tmp_path();
    let () = create_dir(&tmp_path).unwrap();
    let () = create_dir(format!("{}/data", &tmp_path)).unwrap();

    for file in seed_files {
        let other_file_path = format!("{}/data/{}", &tmp_path, &file.name);
        write(&other_file_path, &file.data).unwrap();
    }

    let flags = if is_read_only {
        fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::DIRECTORY
    } else {
        fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE | fio::OpenFlags::DIRECTORY
    };

    fuchsia_fs::directory::open_channel_in_namespace(
        &tmp_path,
        flags,
        ServerEnd::new(server.into_channel()),
    )
    .unwrap();
    Ok(())
}

// Creates files with specified contents within a host directory.
pub fn populate_host_with_file_contents(path: &str, mut seed_files: Vec<File>) -> Result<()> {
    seed_files.retain(|file| file.on_host);

    for file in seed_files.iter() {
        if file.on_host {
            let new_file_path = format!("{}/{}", path, &file.name);
            write(&new_file_path, file.data.as_bytes().to_vec()).unwrap();
        }
    }
    Ok(())
}

// Returns the data within a path in a namespace.
pub async fn read_data_from_namespace(
    ns_proxy: &DirectoryProxy,
    data_path: &str,
) -> Result<Vec<u8>> {
    let ns_dir = Directory::from_proxy(ns_proxy.to_owned());
    let file_data = ns_dir.read_file_bytes(PathBuf::from(data_path)).await.unwrap();
    Ok(file_data)
}
