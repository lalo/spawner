use self::{docker::DockerInterface, executor::Executor};
use crate::{config::DockerConfig, database::DroneDatabase, ip::IpSource};
use anyhow::{anyhow, Result};
use http::Uri;
use hyper::Client;
use plane_core::{
    logging::LogError,
    messages::agent::{DroneConnectRequest, DroneStatusMessage, SpawnRequest, TerminationRequest},
    nats::TypedNats,
    retry::do_with_retry,
    types::{ClusterName, DroneId},
    NeverResult,
};
use std::{net::IpAddr, time::Duration};

mod backend;
mod docker;
mod executor;

pub struct AgentOptions {
    pub drone_id: DroneId,
    pub db: DroneDatabase,
    pub nats: TypedNats,
    pub cluster_domain: ClusterName,

    /// Public IP of the machine the drone is running on.
    pub ip: IpSource,

    pub docker_options: DockerConfig,
}

pub async fn wait_port_ready(port: u16, host_ip: IpAddr) -> Result<()> {
    tracing::info!(port, %host_ip, "Waiting for ready port.");

    let client = Client::new();
    let uri = Uri::from_maybe_shared(format!("http://{}:{}/", host_ip, port))?;

    do_with_retry(|| client.get(uri.clone()), 3000, Duration::from_millis(10)).await?;

    Ok(())
}

async fn listen_for_spawn_requests(
    drone_id: &DroneId,
    executor: Executor,
    nats: TypedNats,
) -> NeverResult {
    let mut sub = nats
        .subscribe(SpawnRequest::subscribe_subject(drone_id))
        .await?;
    executor.resume_backends().await?;
    tracing::info!("Listening for spawn requests.");

    loop {
        let req = sub.next().await;

        match req {
            Ok(Some(req)) => {
                let executor = executor.clone();

                req.respond(&true).await?;
                tokio::spawn(async move {
                    executor.start_backend(&req.value).await;
                });
            }
            Ok(None) => return Err(anyhow!("Spawn request subscription closed.")),
            Err(error) => {
                tracing::error!(?error, "Non-fatal error when listening for spawn requests.")
            }
        }
    }
}

async fn listen_for_termination_requests(executor: Executor, nats: TypedNats) -> NeverResult {
    let mut sub = nats
        .subscribe(TerminationRequest::subscribe_subject())
        .await?;
    tracing::info!("Listening for termination requests.");
    loop {
        let req = sub.next().await;
        match req {
            Ok(Some(req)) => {
                let executor = executor.clone();

                req.respond(&true).await?;
                tokio::spawn(async move { executor.kill_backend(&req.value).await });
            }
            Ok(None) => return Err(anyhow!("Termination request subscription closed.")),
            Err(error) => {
                tracing::error!(
                    ?error,
                    "Non-fatal error when listening for termination requests."
                )
            }
        }
    }
}

/// Repeatedly publish a status message advertising this drone as available.
async fn ready_loop(nc: TypedNats, drone_id: &DroneId, cluster: ClusterName) -> NeverResult {
    let mut interval = tokio::time::interval(Duration::from_secs(4));

    loop {
        nc.publish_jetstream(&DroneStatusMessage {
            drone_id: drone_id.clone(),
            capacity: 100,
            cluster: cluster.clone(),
        })
        .await
        .log_error("Error in ready loop.");

        interval.tick().await;
    }
}

pub async fn run_agent(agent_opts: AgentOptions) -> NeverResult {
    let nats = &agent_opts.nats;

    tracing::info!("Connecting to Docker.");
    let docker = DockerInterface::try_new(&agent_opts.docker_options).await?;
    tracing::info!("Connecting to sqlite.");
    let db = agent_opts.db;
    let cluster = agent_opts.cluster_domain.clone();
    let ip = do_with_retry(|| agent_opts.ip.get_ip(), 10, Duration::from_secs(10)).await?;

    let request = DroneConnectRequest {
        drone_id: agent_opts.drone_id.clone(),
        cluster: cluster.clone(),
        ip,
    };

    nats.publish(&request).await?;

    let executor = Executor::new(docker, db, nats.clone(), ip, cluster.clone());
    tokio::select!(
        result = ready_loop(nats.clone(), &agent_opts.drone_id, cluster.clone()) => result,
        result = listen_for_spawn_requests(&agent_opts.drone_id, executor.clone(), nats.clone()) => result,
        result = listen_for_termination_requests(executor.clone(), nats.clone()) => result
    )
}
