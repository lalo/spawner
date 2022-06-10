use super::DockerOptions;
use anyhow::{anyhow, Result};
use bollard::{
    container::{Config, CreateContainerOptions, StartContainerOptions, StopContainerOptions},
    image::CreateImageOptions,
    models::{EventMessage, HostConfig, PortBinding},
    network::CreateNetworkOptions,
    system::EventsOptions,
    Docker, API_DEFAULT_VERSION,
};
use std::collections::HashMap;
use tokio_stream::{Stream, StreamExt};

/// The port in the container which is exposed.
const CONTAINER_PORT: u16 = 8080;
const DEFAULT_DOCKER_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone)]
pub struct DockerInterface {
    docker: Docker,
    runtime: Option<String>,
}

#[derive(Debug)]
pub enum ContainerEventType {
    Start,
    Create,
    Die { error_status: bool },
    Stop,
    Kill,
}

#[allow(unused)]
#[derive(Debug)]
pub struct ContainerEvent {
    event: ContainerEventType,
    container: String,
}

pub fn backend_name(backend: &str) -> String {
    format!("backend-{}", backend)
}

pub fn network_name(backend: &str) -> String {
    format!("network-{}", backend)
}

impl ContainerEvent {
    pub fn from_event_message(event: &EventMessage) -> Option<Self> {
        let action = event.action.as_deref()?;
        let actor = event.actor.as_ref()?;
        let container = actor.id.as_deref()?.to_string();

        let event = match action {
            "die" => {
                let exit_code: u32 = actor
                    .attributes
                    .as_ref()?
                    .get("exitCode")?
                    .parse()
                    .unwrap_or(1);
                ContainerEventType::Die {
                    error_status: exit_code != 0,
                }
            }
            "stop" => ContainerEventType::Stop,
            "start" => ContainerEventType::Start,
            "kill" => ContainerEventType::Kill,
            "create" => ContainerEventType::Create,
            _ => {
                tracing::info!(?action, "Unhandled container action.");
                return None;
            }
        };

        Some(ContainerEvent { event, container })
    }
}

fn make_exposed_ports(port: u16) -> Option<HashMap<String, HashMap<(), ()>>> {
    let dummy: HashMap<(), ()> = vec![].into_iter().collect();
    Some(vec![(format!("{}/tcp", port), dummy)].into_iter().collect())
}

impl DockerInterface {
    pub async fn try_new(config: &DockerOptions) -> Result<Self> {
        let docker = match &config.transport {
            super::DockerApiTransport::Socket(docker_socket) => Docker::connect_with_unix(
                &docker_socket,
                DEFAULT_DOCKER_TIMEOUT_SECONDS,
                API_DEFAULT_VERSION,
            )?,
            super::DockerApiTransport::Http(docker_http) => Docker::connect_with_http(
                &docker_http,
                DEFAULT_DOCKER_TIMEOUT_SECONDS,
                API_DEFAULT_VERSION,
            )?,
        };

        Ok(DockerInterface {
            docker,
            runtime: config.runtime.clone(),
        })
    }

    pub async fn container_events(&self) -> impl Stream<Item = ContainerEvent> {
        let options: EventsOptions<&str> = EventsOptions {
            since: None,
            until: None,
            filters: vec![("type", vec!["container"])].into_iter().collect(),
        };
        self.docker
            .events(Some(options))
            .filter_map(|event| match event {
                Ok(event) => ContainerEvent::from_event_message(&event),
                Err(error) => {
                    tracing::error!(?error, "Error tracking container terminations.");
                    None
                }
            })
    }

    #[allow(unused)]
    pub async fn pull_image(&self, image: &str) -> Result<()> {
        let options = Some(CreateImageOptions {
            from_image: image,
            ..Default::default()
        });

        let mut result = self.docker.create_image(options, None, None);
        while let Some(next) = result.next().await {
            next?;
        }

        Ok(())
    }

    pub async fn stop_container(&self, name: &str) -> Result<()> {
        let options = StopContainerOptions { t: 10 };

        self.docker
            .stop_container(&backend_name(name), Some(options))
            .await?;

        self.docker.remove_network(&network_name(name)).await?;

        Ok(())
    }

    pub async fn get_port(&self, container_name: &str) -> Option<u16> {
        let inspect = self
            .docker
            .inspect_container(&backend_name(container_name), None)
            .await
            .ok()?;

        let port = inspect
            .network_settings
            .as_ref()?
            .ports
            .as_ref()?
            .get(&format!("{}/tcp", CONTAINER_PORT))?
            .as_ref()?
            .first()?
            .host_port
            .as_ref()?;

        port.parse().ok()
    }

    /// Run the specified image and return the name of the created container.
    pub async fn run_container(
        &self,
        name: &str,
        image: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let env: Vec<String> = env
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Build the network.
        let network_id = {
            let network_name = network_name(name);
            let options: CreateNetworkOptions<&str> = CreateNetworkOptions {
                name: &network_name,
                ..CreateNetworkOptions::default()
            };

            let response = self.docker.create_network(options).await?;
            response
                .id
                .ok_or_else(|| anyhow!("Network response contained no network id."))?
        };

        // Build the container.
        let container_id = {
            let options: Option<CreateContainerOptions<String>> = Some(
                CreateContainerOptions {
                    name: backend_name(name)
                }
            );

            let config: Config<String> = Config {
                image: Some(image.to_string()),
                env: Some(env),
                exposed_ports: make_exposed_ports(CONTAINER_PORT),
                labels: Some(
                    vec![
                        ("dev.spawner.managed".to_string(), "true".to_string()),
                        ("dev.spawner.backend".to_string(), name.to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                host_config: Some(HostConfig {
                    network_mode: Some(network_id.clone()),
                    port_bindings: Some(
                        vec![(
                            format!("{}/tcp", CONTAINER_PORT),
                            Some(vec![PortBinding {
                                host_ip: None,
                                host_port: Some("0".to_string()),
                            }]),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                    runtime: self.runtime.clone(),
                    ..HostConfig::default()
                }),
                ..Config::default()
            };

            let result = self.docker.create_container(options, config).await?;
            result.id
        };

        // Start the container.
        {
            let options: Option<StartContainerOptions<&str>> = None;

            self.docker.start_container(&container_id, options).await?;
        };

        Ok(())
    }
}
