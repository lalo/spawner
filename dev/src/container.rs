use crate::{scratch_dir, TEST_CONTEXT};
use anyhow::Result;
use bollard::{
    container::{Config, LogsOptions, StartContainerOptions},
    image::CreateImageOptions,
    models::HostConfig,
    Docker,
};
use std::{collections::HashMap, fmt::Display, fs::File, io::Write, net::Ipv4Addr, ops::Deref};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub environment: HashMap<String, String>,
    pub command: Vec<String>,
    pub volumes: Vec<(String, String)>,
}

impl ContainerSpec {
    pub fn as_config(&self) -> Config<String> {
        let env: Vec<String> = self
            .environment
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let volumes: Vec<String> = self
            .volumes
            .iter()
            .map(|(s, d)| format!("{}:{}", s, d))
            .collect();

        Config {
            hostname: Some(self.name.to_string()),
            env: Some(env),
            cmd: Some(self.command.clone()),
            image: Some(self.image.to_string()),
            host_config: Some(HostConfig {
                binds: Some(volumes),
                ..HostConfig::default()
            }),
            ..Config::default()
        }
    }
}

#[derive(Clone)]
pub struct ContainerId {
    name: String,
    hint: String,
}

impl ContainerId {
    pub fn new(name: String, hint: String) -> ContainerId {
        ContainerId { name, hint }
    }

    pub fn short_name(&self) -> String {
        self.name.chars().take(12).collect()
    }
}

impl Deref for ContainerId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.name
    }
}

impl Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.short_name(), self.hint)
    }
}

pub struct ContainerResource {
    docker: Docker,
    container_id: ContainerId,
    pub ip: Ipv4Addr,
    #[allow(unused)]
    log_handle: JoinHandle<()>,
}

impl ContainerResource {
    pub async fn new(spec: &ContainerSpec) -> Result<ContainerResource> {
        let docker = Docker::connect_with_unix_defaults()?;

        tracing::info!(image_name=%spec.image, "Pulling container.");
        pull_image(&docker, &spec.image).await?;

        tracing::info!(image_name=%spec.image, "Creating container.");
        let result = docker
            .create_container::<String, String>(None, spec.as_config())
            .await?;
        let container_id = ContainerId::new(result.id, spec.name.clone());

        tracing::info!(%container_id, "Starting container.");
        {
            let options: Option<StartContainerOptions<&str>> = None;
            docker.start_container(&container_id, options).await?;
        };

        let result = docker.inspect_container(&container_id, None).await?;
        let ip: Ipv4Addr = result
            .network_settings
            .unwrap()
            .ip_address
            .unwrap()
            .parse()
            .unwrap();

        tracing::info!(%container_id, %ip, "Container started.");

        let mut log_stream = docker.logs(
            &container_id,
            Some(LogsOptions::<&str> {
                follow: true,
                stderr: true,
                stdout: true,
                timestamps: true,
                ..LogsOptions::default()
            }),
        );

        let mut log_file = File::create(scratch_dir("logs").join(format!("{}.txt", spec.name)))?;

        let log_handle = tokio::spawn(async move {
            while let Some(Ok(v)) = log_stream.next().await {
                log_file.write_all(&v.into_bytes()).unwrap();
            }
        });

        Ok(ContainerResource {
            docker,
            container_id,
            ip,
            log_handle,
        })
    }
}

impl Drop for ContainerResource {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let container_id = self.container_id.clone();

        TEST_CONTEXT.with(|manager| {
            manager
                .borrow()
                .as_ref()
                .unwrap()
                .add_teardown_task(async move {
                    tracing::info!(%container_id, "Stopping container");
                    docker
                        .stop_container(&container_id, None)
                        .await
                        .expect("Error stopping container.");
                    tracing::info!(%container_id, "Removing container");
                    docker
                        .remove_container(&container_id, None)
                        .await
                        .expect("Error removing container.");
                    Ok(())
                })
        });
    }
}

/// White out a line and return to the beginning. The number of spaces is not scientific;
const CLEAR_LINE: &str =
    "                                                                                        \r";

pub async fn pull_image(docker: &Docker, name: &str) -> Result<()> {
    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: name.to_string(),
            ..CreateImageOptions::default()
        }),
        None,
        None,
    );

    while let Some(pull_event) = stream.next().await {
        if let Some(event_message) = pull_event?.status {
            let event_message = event_message.trim();
            print!("{}", CLEAR_LINE);
            print!("{}", event_message);
        }
    }
    println!();

    Ok(())
}
