// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    cli::ServerCommand,
    errors::{Error, ErrorKind, Result},
};
use crate::{
    cli::ServerRun,
    util::{get_source_kind, SourceKind},
};
use async_std::task;
use std::io::Write;
use std::sync::atomic::Ordering;
use tremor_api as api;
use tremor_common::file;
use tremor_runtime::system::World;
use tremor_runtime::{self, version};

impl ServerCommand {
    pub(crate) fn run(&self) {
        match self {
            ServerCommand::Run(c) => c.run(),
        }
    }
}
impl ServerRun {
    pub(crate) fn run(&self) {
        version::print();
        if let Err(ref e) = task::block_on(self.run_dun()) {
            error!("error: {}", e);
            for e in e.iter().skip(1) {
                error!("error: {}", e);
            }
            error!("We are SHUTTING DOWN due to errors during initialization!");

            // ALLOW: main.rs
            ::std::process::exit(1);
        }
    }
    #[cfg(not(tarpaulin_include))]
    pub(crate) async fn run_dun(&self) -> Result<()> {
        // Logging
        if let Some(logger_config) = &self.logger_config {
            log4rs::init_file(logger_config, log4rs::config::Deserializers::default())?;
        } else {
            env_logger::init();
        }
        version::log();
        eprintln!("allocator: {}", crate::alloc::get_allocator_name());

        #[cfg(feature = "bert")]
        {
            let d = tch::Device::cuda_if_available();
            if d.is_cuda() {
                eprintln!("CUDA is supported");
            } else {
                eprintln!("CUDA is NOT  supported, falling back to the CPU");
            }
        }
        if let Some(pid_file) = &self.pid {
            let mut file = file::create(pid_file).map_err(|e| {
                Error::from(format!("Failed to create pid file `{}`: {}", pid_file, e))
            })?;

            file.write(format!("{}\n", std::process::id()).as_ref())
                .map_err(|e| {
                    Error::from(format!("Failed to write pid to `{}`: {}", pid_file, e))
                })?;
        }

        tremor_script::RECURSION_LIMIT.store(self.recursion_limit, Ordering::Relaxed);

        // TODO: Allow configuring this for offramps and pipelines
        let (world, handle) = World::start(64).await?;

        let mut yaml_files = Vec::with_capacity(16);
        // We process trickle files first
        for config_file in &self.artefacts {
            let kind = get_source_kind(config_file);
            match kind {
                SourceKind::Trickle => {
                    if let Err(e) = tremor_runtime::load_query_file(&world, config_file).await {
                        return Err(ErrorKind::FileLoadError(config_file.to_string(), e).into());
                    }
                }
                SourceKind::Tremor | SourceKind::Json | SourceKind::Unsupported(_) => {
                    return Err(ErrorKind::UnsupportedFileType(
                        config_file.to_string(),
                        kind,
                        "yaml",
                    )
                    .into());
                }
                SourceKind::Yaml => yaml_files.push(config_file),
            };
        }

        // We process config files thereafter
        for config_file in yaml_files {
            if let Err(e) = tremor_runtime::load_cfg_file(&world, config_file).await {
                return Err(ErrorKind::FileLoadError(config_file.to_string(), e).into());
            }
        }

        if !self.no_api {
            let app = api_server(&world);
            eprintln!("Listening at: http://{}", &self.api_host);
            info!("Listening at: http://{}", &self.api_host);

            if let Err(e) = app.listen(&self.api_host).await {
                return Err(format!("API Error: {}", e).into());
            }
            warn!("API stopped");
            world.stop().await?;
        }

        handle.await?;
        warn!("World stopped");
        Ok(())
    }
}

async fn handle_api_request<
    G: std::future::Future<Output = api::Result<tide::Response>>,
    F: Fn(api::Request) -> G,
>(
    req: api::Request,
    handler_func: F,
) -> tide::Result {
    let resource_type = api::accept(&req);

    // Handle request. If any api error is returned, serialize it into a tide response
    // as well, respecting the requested resource type. (and if there's error during
    // this serialization, fall back to the error's conversion into tide response)
    handler_func(req).await.or_else(|api_error| {
        api::serialize_error(resource_type, api_error)
            .or_else(|e| Ok(Into::<tide::Response>::into(e)))
    })
}

fn api_server(world: &World) -> tide::Server<api::State> {
    let mut app = tide::Server::with_state(api::State {
        world: world.clone(),
    });

    app.at("/version")
        .get(|r| handle_api_request(r, api::version::get));
    app.at("/binding")
        .get(|r| handle_api_request(r, api::binding::list_artefact))
        .post(|r| handle_api_request(r, api::binding::publish_artefact));
    app.at("/binding/:aid")
        .get(|r| handle_api_request(r, api::binding::get_artefact))
        .delete(|r| handle_api_request(r, api::binding::unpublish_artefact));
    app.at("/binding/:aid/:sid")
        .get(|r| handle_api_request(r, api::binding::get_servant))
        .post(|r| handle_api_request(r, api::binding::link_servant))
        .delete(|r| handle_api_request(r, api::binding::unlink_servant));
    app.at("/pipeline")
        .get(|r| handle_api_request(r, api::pipeline::list_artefact))
        .post(|r| handle_api_request(r, api::pipeline::publish_artefact));
    app.at("/pipeline/:aid")
        .get(|r| handle_api_request(r, api::pipeline::get_artefact))
        .delete(|r| handle_api_request(r, api::pipeline::unpublish_artefact));
    app.at("/onramp")
        .get(|r| handle_api_request(r, api::onramp::list_artefact))
        .post(|r| handle_api_request(r, api::onramp::publish_artefact));
    app.at("/onramp/:aid")
        .get(|r| handle_api_request(r, api::onramp::get_artefact))
        .delete(|r| handle_api_request(r, api::onramp::unpublish_artefact));
    app.at("/offramp")
        .get(|r| handle_api_request(r, api::offramp::list_artefact))
        .post(|r| handle_api_request(r, api::offramp::publish_artefact));
    app.at("/offramp/:aid")
        .get(|r| handle_api_request(r, api::offramp::get_artefact))
        .delete(|r| handle_api_request(r, api::offramp::unpublish_artefact));

    app
}
