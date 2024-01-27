use std::path::PathBuf;
use std::{fs, io};

use clap::Parser;
use dashmap::DashMap;
use tracing::level_filters::LevelFilter;
use tracing::subscriber;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, Registry};

use crate::client::{Client, ListenAddr};
use crate::config::{ClientConfig, Config, Mode, ServerConfig};
use crate::server::{Peer, Server, TlsConfig};

mod auth;
mod client;
mod config;
mod server;

mod proto {
    tonic::include_proto!("hportal");
}

/// hportal is a simple reverse proxy
#[derive(Debug, Parser)]
struct Args {
    /// config path
    #[clap(short, long)]
    config: PathBuf,

    /// enable debug log
    #[clap(short, long, action)]
    debug: bool,
}

pub async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    init_log(args.debug);

    let config = fs::read(&args.config)?;
    let config = serde_yaml::from_slice::<Config>(&config)?;

    match config.mode {
        Mode::Client => {
            let client_config = config.client.expect("client mode must set client config");

            run_client(client_config).await
        }
        Mode::Server => {
            let server_config = config.server.expect("server mode must set client config");

            run_server(server_config).await
        }
    }
}

async fn run_server(server_config: ServerConfig) -> anyhow::Result<()> {
    let peers = server_config
        .peers
        .into_iter()
        .map(|peer_config| {
            let peer = Peer::new(peer_config.secret)?;

            Ok((peer_config.name, peer))
        })
        .collect::<anyhow::Result<DashMap<_, _>>>()?;

    let tls_config = match server_config.tls {
        None => None,
        Some(tls) => {
            let key = fs::read(tls.key)?;
            let cert = fs::read(tls.cert)?;

            Some(TlsConfig::new(cert, key))
        }
    };

    Server::run(server_config.server_addr, peers, tls_config).await
}

async fn run_client(client_config: ClientConfig) -> anyhow::Result<()> {
    if client_config.forwards.is_empty() {
        return Err(anyhow::anyhow!("empty forwards"));
    }

    let listen_addrs = client_config
        .forwards
        .into_iter()
        .map(|forward| {
            let listen_addr = ListenAddr::new(forward.local_addr, forward.remote_addr);

            (forward.name, listen_addr)
        })
        .collect();

    let mut client = Client::new(
        client_config.name,
        client_config.secret,
        client_config.server_addr,
        listen_addrs,
    )
    .await?;

    client.run().await;

    Err(anyhow::anyhow!("client stop unexpectedly"))
}

pub fn init_log(debug: bool) {
    let layer = fmt::layer()
        .pretty()
        .with_target(true)
        .with_writer(io::stderr);

    let level = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };

    let targets = Targets::new()
        .with_target("h2", LevelFilter::OFF)
        .with_default(LevelFilter::DEBUG);

    let layered = Registry::default().with(targets).with(layer).with(level);

    subscriber::set_global_default(layered).unwrap();
}
