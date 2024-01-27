use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub client: Option<ClientConfig>,
    pub server: Option<ServerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Client,
    Server,
}

#[derive(Debug, Deserialize)]
pub struct ClientConfig {
    pub name: String,
    pub secret: String,
    pub server_addr: String,
    pub forwards: Vec<ForwardAddrAndPort>,
}

#[derive(Debug, Deserialize)]
pub struct ForwardAddrAndPort {
    pub name: String,
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server_addr: SocketAddr,
    pub peers: Vec<PeerConfig>,
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Deserialize)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct PeerConfig {
    pub name: String,
    pub secret: String,
}
