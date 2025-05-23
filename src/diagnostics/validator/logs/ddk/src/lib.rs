// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use diagnostics_log_types::Severity;
use diagnostics_reader::{ArchiveReader, Data, Logs, SubscriptionResultsStream};
use fidl_fuchsia_validate_logs::{
    Argument, LogSinkPuppetProxy, PuppetInfo, Record, RecordSpec, Value,
};
use fuchsia_async::Task;
use futures::StreamExt;
use log::*;

struct Puppet {
    _info: PuppetInfo,
    proxy: LogSinkPuppetProxy,
    _reader_errors_task: Task<()>,
    logs: SubscriptionResultsStream<Data<Logs>>,
}

impl Puppet {
    // Creates a Puppet instance.
    // Since this is v2, there is no URL to spawn as we are using RealmBuilder.
    async fn launch(proxy: fidl_fuchsia_validate_logs::LogSinkPuppetProxy) -> Result<Self, Error> {
        info!("Requesting info from the puppet.");
        let info = proxy.get_info().await?;
        let reader = ArchiveReader::logs();
        let (logs, mut errors) = reader.snapshot_then_subscribe().unwrap().split_streams();
        let task = Task::spawn(async move {
            if let Some(e) = errors.next().await {
                panic!("error in subscription: {e}");
            }
        });
        Ok(Self { proxy, _info: info, _reader_errors_task: task, logs })
    }

    async fn test_puppet_started(&mut self) -> Result<(), Error> {
        info!("Ensuring we received the init message.");

        loop {
            let log_entry = self.logs.next().await.unwrap();
            if log_entry.msg().unwrap().contains("Puppet started.") {
                break;
            }
        }
        info!("Got init message");
        Ok(())
    }

    async fn test_basic_log(&mut self) -> Result<(), Error> {
        let test_log = "test_log";
        let test_file = "test_file.cc".to_string();
        let test_line_64: u64 = 9001;
        let test_line_32 = 9001;
        // TODO(https://fxbug.dev/42145848): Additional arguments aren't yet supported by the DDK.
        let record = Record {
            arguments: vec![Argument {
                name: "message".to_string(),
                value: Value::Text(test_log.to_string()),
            }],
            severity: Severity::Error.into(),
            timestamp: zx::BootInstant::ZERO,
        };
        let spec = RecordSpec { file: test_file.clone(), line: test_line_32, record };
        self.proxy.emit_log(&spec).await?;
        info!("Sent message");
        loop {
            let log_entry = self.logs.next().await.unwrap();
            let has_msg = log_entry.msg().unwrap().contains(test_log);
            let has_file = match log_entry.file_path() {
                None => false,
                Some(file) => file == test_file.clone(),
            };
            let has_line = match log_entry.line_number() {
                None => false,
                Some(line) => *line == test_line_64,
            };
            if has_msg && has_file && has_line {
                break;
            }
        }
        info!("Tested LogSink socket successfully.");
        Ok(())
    }

    async fn test_file_line(&mut self) -> Result<(), Error> {
        let test_file = "test_file.cc".to_string();
        let test_line_32 = 9001;
        let long_test_log = "test_log_".repeat(1000);
        let record = Record {
            arguments: vec![Argument {
                name: "message".to_string(),
                value: Value::Text(long_test_log.to_string()),
            }],
            severity: Severity::Info.into(),
            timestamp: zx::BootInstant::ZERO,
        };
        let spec = RecordSpec { file: test_file, line: test_line_32, record };
        self.proxy.emit_log(&spec).await?;
        info!("Sent message");
        loop {
            let log_entry = self.logs.next().await.unwrap();
            let has_msg = log_entry.msg().unwrap().contains(&"test_log_".repeat(110).to_owned());
            if has_msg {
                break;
            }
        }
        info!("Tested file/line number successfully.");
        Ok(())
    }

    async fn test(&mut self) -> Result<(), Error> {
        self.test_puppet_started().await?;
        self.test_basic_log().await?;
        self.test_file_line().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fidl_fuchsia_driver_test as fdt;
    use fuchsia_component_test::RealmBuilder;
    use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};

    #[fuchsia::test]
    async fn log_test() {
        let realm = RealmBuilder::new().await.unwrap();
        let _ = realm.driver_test_realm_setup().await.unwrap();
        let realm = realm.build().await.expect("failed to build realm");
        realm.driver_test_realm_start(fdt::RealmArgs::default()).await.unwrap();
        let out_dir = realm.driver_test_realm_connect_to_dev().unwrap();

        let driver_proxy = device_watcher::recursive_wait_and_open::<
            fidl_fuchsia_validate_logs::LogSinkPuppetMarker,
        >(&out_dir, "sys/test/virtual-logsink")
        .await
        .unwrap();
        let mut puppet = Puppet::launch(driver_proxy).await.unwrap();
        puppet.test().await.unwrap();
    }
}
