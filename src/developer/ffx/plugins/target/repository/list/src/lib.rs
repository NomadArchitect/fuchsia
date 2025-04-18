// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use ffx_target_repository_list_args::ListCommand;
use ffx_writer::{ToolIO as _, VerifiedMachineWriter};
use fho::{bug, Error, FfxMain, FfxTool, Result};
use fidl_fuchsia_pkg::RepositoryKeyConfig::Ed25519Key;
use fidl_fuchsia_pkg::{RepositoryConfig, RepositoryManagerProxy};
use fidl_fuchsia_pkg_ext::RepositoryStorageType;
use fidl_fuchsia_pkg_rewrite::EngineProxy;
use fidl_fuchsia_pkg_rewrite_ext::Rule;
use prettytable::format::FormatBuilder;
use prettytable::{cell, row, Table};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use target_holders::moniker;

#[derive(FfxTool)]
pub struct ListTool {
    #[command]
    _cmd: ListCommand,
    #[with(moniker(REPOSITORY_MANAGER_MONIKER))]
    repo_proxy: RepositoryManagerProxy,
    #[with(moniker(REPOSITORY_MANAGER_MONIKER))]
    engine_proxy: EngineProxy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepositoryInfo {
    pub name: String,
    pub keys: Vec<String>,
    pub mirrors: Vec<String>,
    pub root_version: Option<u32>,
    pub root_threshold: Option<u32>,
    pub storage_type: Option<RepositoryStorageType>,
    pub aliases: Vec<String>,
}

impl From<RepositoryConfig> for RepositoryInfo {
    fn from(value: RepositoryConfig) -> Self {
        let mirrors: Vec<String> = if let Some(mirror_list) = value.mirrors {
            mirror_list
                .iter()
                .filter_map(
                    |x| if let Some(u) = &x.mirror_url { Some(u.to_string()) } else { None },
                )
                .collect()
        } else {
            vec![]
        };
        let keys = if let Some(key_list) = value.root_keys {
            key_list
                .iter()
                .filter_map(|x| {
                    if let Ed25519Key(data) = x {
                        #[allow(
                            clippy::format_collect,
                            reason = "mass allow for https://fxbug.dev/381896734"
                        )]
                        Some(data.iter().map(|c| format!("{c:02x}")).collect::<String>())
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            vec![]
        };
        RepositoryInfo {
            name: value.repo_url.unwrap_or_default(),
            keys,
            mirrors,
            root_version: value.root_version,
            root_threshold: value.root_threshold,
            storage_type: match value.storage_type {
                Some(fidl_fuchsia_pkg::RepositoryStorageType::Ephemeral) => {
                    Some(RepositoryStorageType::Ephemeral)
                }
                Some(fidl_fuchsia_pkg::RepositoryStorageType::Persistent) => {
                    Some(RepositoryStorageType::Persistent)
                }
                None => None,
            },
            aliases: vec![],
        }
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Successfully waited for the target (either to come up or shut down).
    Ok { data: Vec<RepositoryInfo> },
    /// Unexpected error with string denoting error message.
    UnexpectedError { message: String },
    /// A known error that can be reported to the user.
    UserError { message: String },
}

const REPOSITORY_MANAGER_MONIKER: &str = "/core/pkg-resolver";

fho::embedded_plugin!(ListTool);
#[async_trait(?Send)]
impl FfxMain for ListTool {
    type Writer = VerifiedMachineWriter<CommandStatus>;
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        match self.list_from_device().await {
            Ok(info) => {
                if writer.is_machine() {
                    writer.machine(&CommandStatus::Ok { data: info })?;
                } else {
                    ListTool::write_info(&info, &mut writer)?;
                }
                Ok(())
            }
            Err(e @ Error::User(_)) => {
                writer.machine(&CommandStatus::UserError { message: e.to_string() })?;
                Err(e)
            }
            Err(e) => {
                writer.machine(&CommandStatus::UnexpectedError { message: e.to_string() })?;
                Err(e)
            }
        }
    }
}

impl ListTool {
    async fn list_from_device(&self) -> fho::Result<Vec<RepositoryInfo>> {
        let (repo_iterator, repo_iterator_server) = fidl::endpoints::create_proxy();
        self.repo_proxy.list(repo_iterator_server).map_err(|e| bug!(e))?;
        let mut ret: Vec<RepositoryInfo> = vec![];
        loop {
            let repos = repo_iterator.next().await.map_err(|e| bug!(e))?;
            if repos.is_empty() {
                break;
            }
            ret.extend(repos.into_iter().map(|r| r.try_into().unwrap()))
        }

        let (rule_iterator, rule_iterator_server) = fidl::endpoints::create_proxy();
        self.engine_proxy.list(rule_iterator_server).map_err(|e| bug!("{e}"))?;

        let mut rewrite_rules: Vec<Rule> = vec![];
        loop {
            let rules = rule_iterator.next().await.map_err(|e| bug!(e))?;
            if rules.is_empty() {
                break;
            }
            rewrite_rules.extend(rules.into_iter().map(|r| r.try_into().unwrap()));
        }

        for repo in &mut ret {
            let aliases = rewrite_rules
                .iter()
                .filter_map(|r| {
                    if repo.name.ends_with(r.host_replacement()) {
                        Some(r.host_match().to_string())
                    } else {
                        None
                    }
                })
                .collect();
            repo.aliases = aliases;
        }
        Ok(ret)
    }

    fn write_info(
        info: &Vec<RepositoryInfo>,
        writer: &mut <ListTool as fho::FfxMain>::Writer,
    ) -> fho::Result<()> {
        if info.is_empty() {
            return Ok(());
        }

        let mut table = Table::new();

        // display_lengths requires right padding
        let table_format = FormatBuilder::new().padding(/*left*/ 0, /*right*/ 1).build();
        table.set_format(table_format);

        table.set_titles(row!("REPO", "URL", "ALIASES"));

        for repo in info {
            table.add_row(row!(
                repo.name,
                format!("{:?}", repo.mirrors),
                format!("{:?}", repo.aliases)
            ));
        }

        table.print(writer).map_err(|e| bug!(e))?;
        Ok(())
    }
}
#[cfg(test)]
mod test {
    use super::*;
    use ffx_writer::{Format, TestBuffers};
    use fidl_fuchsia_pkg::RepositoryManagerRequest;
    use fidl_fuchsia_pkg_rewrite::{EditTransactionRequest, EngineRequest, RuleIteratorRequest};
    use futures::TryStreamExt;
    use target_holders::fake_proxy;

    macro_rules! rule {
        ($host_match:expr => $host_replacement:expr,
         $path_prefix_match:expr => $path_prefix_replacement:expr) => {
            Rule::new($host_match, $host_replacement, $path_prefix_match, $path_prefix_replacement)
                .unwrap()
        };
    }

    async fn setup_fake_repo_proxy(repos: Vec<RepositoryConfig>) -> RepositoryManagerProxy {
        let repos = fake_proxy(move |req| match req {
            RepositoryManagerRequest::Add { repo: _, responder } => {
                responder.send(Ok(())).unwrap();
            }
            RepositoryManagerRequest::List { iterator, .. } => {
                let repos = repos.clone();
                fuchsia_async::Task::local(async move {
                    let mut sent = false;
                    let mut stream = iterator.into_stream();
                    while let Some(req) = stream.try_next().await.unwrap() {
                        let fidl_fuchsia_pkg::RepositoryIteratorRequest::Next { responder } = req;
                        if !sent {
                            sent = true;
                            responder.send(&repos).unwrap();
                        } else {
                            responder.send(&[]).unwrap();
                        }
                    }
                })
                .detach();
            }
            other => panic!("Unexpected request: {:?}", other),
        });
        repos
    }

    async fn setup_fake_engine_proxy(rules: Vec<fidl_fuchsia_pkg_rewrite::Rule>) -> EngineProxy {
        let engine = fake_proxy(move |req| match req {
            EngineRequest::StartEditTransaction { transaction, control_handle: _ } => {
                fuchsia_async::Task::local(async move {
                    let mut tx_stream = transaction.into_stream();
                    while let Some(req) = tx_stream.try_next().await.unwrap() {
                        match req {
                            EditTransactionRequest::ResetAll { control_handle: _ } => (),
                            EditTransactionRequest::ListDynamic { iterator, control_handle: _ } => {
                                let mut stream = iterator.into_stream();

                                while let Some(req) = stream.try_next().await.unwrap() {
                                    let RuleIteratorRequest::Next { responder } = req;
                                    responder.send(&[]).unwrap();
                                }
                            }
                            EditTransactionRequest::Add { rule: _, responder } => {
                                responder.send(Ok(())).unwrap();
                            }
                            EditTransactionRequest::Commit { responder } => {
                                responder.send(Ok(())).unwrap();
                            }
                        }
                    }
                })
                .detach()
            }
            EngineRequest::List { iterator, .. } => {
                let rules = rules.clone();
                fuchsia_async::Task::local(async move {
                    let mut stream = iterator.into_stream();
                    let mut sent = false;
                    while let Some(req) = stream.try_next().await.unwrap() {
                        let RuleIteratorRequest::Next { responder } = req;
                        if !sent {
                            sent = true;
                            responder.send(rules.as_slice()).unwrap();
                        } else {
                            responder.send(&[]).unwrap();
                        };
                    }
                })
                .detach();
            }
            other => panic!("Unexpected request: {:?}", other),
        });
        engine
    }

    #[fuchsia::test]
    async fn list_table() {
        let test_buffers = TestBuffers::default();
        let writer = <ListTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let tool = ListTool {
            _cmd: ListCommand {},
            repo_proxy: setup_fake_repo_proxy(vec![
                RepositoryConfig {
                    repo_url: Some("fuchsia-pkg://bob".into()),
                    ..Default::default()
                },
                RepositoryConfig {
                    repo_url: Some("fuchsia-pkg://smith".into()),
                    ..Default::default()
                },
            ])
            .await,
            engine_proxy: setup_fake_engine_proxy(vec![
                rule!( "target1-alias1" => "bob", "/" => "/").into(),
                rule!( "target1-alias2" => "bob", "/" => "/").into(),
            ])
            .await,
        };
        tool.main(writer).await.expect("main ok");
        let expected = concat!(
            "REPO                URL ALIASES \n",
            "fuchsia-pkg://bob   []  [\"target1-alias1\", \"target1-alias2\"] \n",
            "fuchsia-pkg://smith []  [] \n"
        )
        .to_owned();

        assert_eq!(expected, test_buffers.into_stdout_str());
    }

    #[fuchsia::test]
    async fn list_json() {
        let test_buffers = TestBuffers::default();
        let writer =
            <ListTool as fho::FfxMain>::Writer::new_test(Some(Format::Json), &test_buffers);
        let tool = ListTool {
            _cmd: ListCommand {},
            repo_proxy: setup_fake_repo_proxy(vec![
                RepositoryConfig {
                    repo_url: Some("fuchsia-pkg://bob".into()),
                    ..Default::default()
                },
                RepositoryConfig {
                    repo_url: Some("fuchsia-pkg://smith".into()),
                    storage_type: Some(RepositoryStorageType::Ephemeral.into()),
                    ..Default::default()
                },
            ])
            .await,
            engine_proxy: setup_fake_engine_proxy(vec![
                rule!( "target1-alias1" => "bob", "/" => "/").into(),
                rule!( "target1-alias2" => "bob", "/" => "/").into(),
            ])
            .await,
        };
        tool.main(writer).await.expect("main ok");
        static EXPECT: &str = "{\"ok\":{\"data\":[{\"name\":\"fuchsia-pkg://bob\",\"keys\":[],\
        \"mirrors\":[],\"root_version\":null,\"root_threshold\":null,\"storage_type\":null,\
        \"aliases\":[\"target1-alias1\",\"target1-alias2\"]},{\"name\":\"fuchsia-pkg://smith\",\
        \"keys\":[],\"mirrors\":[],\"root_version\":null,\"root_threshold\":null,\
        \"storage_type\":\"ephemeral\",\"aliases\":[]}]}}\n";
        assert_eq!(EXPECT, (test_buffers.into_stdout_str()));
    }

    #[fuchsia::test]
    async fn list_empty() {
        let test_buffers = TestBuffers::default();
        let writer = <ListTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let tool = ListTool {
            _cmd: ListCommand {},
            repo_proxy: setup_fake_repo_proxy(vec![]).await,
            engine_proxy: setup_fake_engine_proxy(vec![]).await,
        };
        tool.main(writer).await.expect("main ok");
        static EXPECT: &str = "";
        assert_eq!(EXPECT, (test_buffers.into_stdout_str()));
    }

    #[fuchsia::test]
    async fn list_json_empty() {
        let test_buffers = TestBuffers::default();
        let writer =
            <ListTool as fho::FfxMain>::Writer::new_test(Some(Format::Json), &test_buffers);
        let tool = ListTool {
            _cmd: ListCommand {},
            repo_proxy: setup_fake_repo_proxy(vec![]).await,
            engine_proxy: setup_fake_engine_proxy(vec![]).await,
        };
        tool.main(writer).await.expect("main ok");

        static EXPECT: &str = "{\"ok\":{\"data\":[]}}\n";
        assert_eq!(EXPECT, (test_buffers.into_stdout_str()));
    }
}
