use std::collections::HashMap;
use std::io::Error;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::BytesMut;
use dashmap::{DashMap, DashSet};
use defer::defer;
use flume::r#async::RecvStream;
use flume::Sender;
use futures_util::future::{abortable, AbortHandle};
use futures_util::{stream, Stream, StreamExt, TryStreamExt};
use rand::random;
use tap::TapFallible;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::{Identity, ServerTlsConfig};
use tonic::{transport, Request, Response, Status, Streaming};
use tracing::{debug, error, info, instrument};

use crate::auth::Auth;
use crate::proto::connect_result_or_data::Body;
use crate::proto::hportal_server::{Hportal, HportalServer};
use crate::proto::{
    ConnectData, ConnectRequest, ConnectResult, ConnectResultOrData, ListenRequest,
};

#[derive(Debug)]
pub struct Peer {
    auth: Auth,
    listen_addrs: HashMap<String, SocketAddr>,
    abort_handle: Option<AbortHandle>,
}

impl Peer {
    pub fn new(secret: String) -> anyhow::Result<Self> {
        Ok(Self {
            auth: Auth::new(secret)?,
            listen_addrs: Default::default(),
            abort_handle: None,
        })
    }
}

#[derive(Debug)]
pub struct TlsConfig {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl TlsConfig {
    pub fn new(cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self { cert, key }
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

#[derive(Debug)]
struct ServerInner {
    peers: DashMap<String, Peer>,
    listened_addrs: DashSet<SocketAddr>,
    wait_establish_tcp_stream: DashMap<u64, TcpStream>,
}

impl Server {
    pub async fn run(
        server_addr: SocketAddr,
        peers: DashMap<String, Peer>,
        tls_config: Option<TlsConfig>,
    ) -> anyhow::Result<()> {
        let server = Self {
            inner: Arc::new(ServerInner {
                peers,
                listened_addrs: Default::default(),
                wait_establish_tcp_stream: Default::default(),
            }),
        };

        let mut builder = transport::Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(60)))
            .http2_keepalive_timeout(Some(Duration::from_secs(180)));
        if let Some(tls_config) = tls_config {
            builder = builder.tls_config(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(tls_config.cert, tls_config.key)),
            )?;
        }

        info!(%server_addr, "start hportal service");

        builder
            .add_service(HportalServer::new(server))
            .serve(server_addr)
            .await?;

        Err(anyhow::anyhow!("server stop unexpectedly"))
    }

    async fn create_listen_future(
        inner: Arc<ServerInner>,
        listen_addrs: HashMap<String, SocketAddr>,
        connect_req_tx: Sender<Result<ConnectRequest, Status>>,
    ) -> anyhow::Result<()> {
        let wait_clean_addrs = listen_addrs.values().copied().collect::<Vec<_>>();

        defer! {
            for addr in wait_clean_addrs {
                inner.listened_addrs.remove(&addr);
            }
        }

        let mut listeners = Vec::with_capacity(listen_addrs.len());
        for (name, addr) in listen_addrs {
            let listener = TcpListener::bind(addr)
                .await
                .tap_err(|err| error!(%err, name, %addr, "tcp listen failed"))?;

            listeners.push(TcpListenerAddrStream { name, listener });
        }

        debug!(?listeners, "tcp listen done");

        let mut listeners = stream::select_all(listeners);

        while let Some(res) = listeners.next().await {
            match res {
                Err(err) => {
                    error!(%err, "tcp accept failed");

                    continue;
                }

                Ok((name, tcp_stream)) => {
                    debug!(name, "accept new connect tcp");

                    let id = random();
                    if connect_req_tx
                        .send_async(Ok(ConnectRequest { name, id }))
                        .await
                        .is_err()
                    {
                        error!("send connect request failed");

                        return Err(anyhow::anyhow!("send connect request failed"));
                    }

                    inner.wait_establish_tcp_stream.insert(id, tcp_stream);
                }
            }
        }

        Err(anyhow::anyhow!("tcp listener stop unexpectedly"))
    }

    #[instrument(level = "debug", err(Debug))]
    async fn handle_listen(
        &self,
        request: Request<ListenRequest>,
    ) -> Result<Response<<Self as Hportal>::ListenStream>, Status> {
        let ListenRequest {
            client_name,
            token,
            listen_addr_and_ports,
        } = request.into_inner();
        let mut peer = match self.inner.peers.get_mut(&client_name) {
            None => {
                error!("unknown client name");

                return Err(Status::unauthenticated("invalid client name or token"));
            }

            Some(peer) => peer,
        };

        if !peer.auth.auth(&token) {
            return Err(Status::unauthenticated("invalid client name or token"));
        }

        if let Some(abort_handle) = peer.abort_handle.take() {
            abort_handle.abort();

            for (_, listen_addr) in peer.listen_addrs.drain() {
                self.inner.listened_addrs.remove(&listen_addr);
            }

            debug!("abort listened client");
        }

        info!("get new listen request");

        for listen_addr_and_port in listen_addr_and_ports {
            let ip = match listen_addr_and_port.remote_addr.parse::<IpAddr>() {
                Err(err) => {
                    return Err(Status::invalid_argument(format!(
                        "parse ip {} failed: {err}",
                        listen_addr_and_port.remote_addr
                    )));
                }

                Ok(ip) => ip,
            };

            let addr = SocketAddr::new(ip, listen_addr_and_port.remote_port as _);
            if self.inner.listened_addrs.contains(&addr) {
                return Err(Status::invalid_argument(format!(
                    "duplicated remote addr: {addr}"
                )));
            }

            self.inner.listened_addrs.insert(addr);
            peer.listen_addrs.insert(listen_addr_and_port.name, addr);
        }

        debug!("check listen_addr_and_ports done");

        let inner = self.inner.clone();
        let listen_addrs = peer.listen_addrs.clone();
        let (connect_req_tx, connect_req_rx) = flume::bounded(10);
        let (fut, abort_handle) = abortable(Self::create_listen_future(
            inner,
            listen_addrs,
            connect_req_tx,
        ));
        peer.abort_handle.replace(abort_handle);
        drop(peer);

        tokio::spawn(fut);

        Ok(Response::new(connect_req_rx.into_stream()))
    }

    #[instrument(level = "debug", err(Debug))]
    async fn handle_establish(
        &self,
        request: Request<Streaming<ConnectResultOrData>>,
    ) -> Result<Response<<Self as Hportal>::EstablishStream>, Status> {
        let mut connect_data_stream = request.into_inner();
        let connect_data = match connect_data_stream
            .try_next()
            .await
            .tap_err(|err| error!(%err, "get first connect data failed"))?
        {
            None => {
                return Err(Status::invalid_argument("connect_data_stream has nothing"));
            }

            Some(connect_data) => connect_data,
        };

        let mut tcp_stream = match connect_data.body {
            None => {
                return Err(Status::invalid_argument("connect data body is none"));
            }

            Some(Body::Data(_)) => {
                return Err(Status::invalid_argument("no connect id found"));
            }

            Some(Body::Result(ConnectResult { id, fail_reason })) => {
                let tcp_stream = match self.inner.wait_establish_tcp_stream.remove(&id) {
                    None => {
                        return Err(Status::invalid_argument("unknown connect id"));
                    }

                    Some((_, tcp_stream)) => tcp_stream,
                };

                if let Some(fail_reason) = fail_reason {
                    return Err(Status::invalid_argument(format!(
                        "connect fail: {fail_reason}"
                    )));
                }

                tcp_stream
            }
        };

        let (data_tx, data_rx) = flume::bounded(1);
        tokio::spawn(async move {
            let (mut tcp_read, mut tcp_write) = tcp_stream.split();

            let fut1 = async {
                while let Some(data) = connect_data_stream
                    .try_next()
                    .await
                    .tap_err(|err| error!(%err, "receive data from stream failed"))?
                {
                    match data.body {
                        Some(Body::Data(data)) => {
                            tcp_write
                                .write_all(&data)
                                .await
                                .tap_err(|err| error!(%err, "tcp write failed"))?;
                        }

                        _ => {
                            error!("data body is invalid");

                            break;
                        }
                    }
                }

                debug!("connect data stream is closed");

                let _ = tcp_write.shutdown().await;

                Ok::<_, anyhow::Error>(())
            };

            let fut2 = async {
                let mut buf = BytesMut::with_capacity(16 * 1024);
                loop {
                    if buf.capacity() < 8 * 1204 {
                        buf.reserve(8 * 1204);
                    }

                    let n = tcp_read
                        .read_buf(&mut buf)
                        .await
                        .tap_err(|err| error!(%err, "tcp read failed"))?;
                    if n == 0 {
                        drop(data_tx);

                        debug!("tcp read is closed");

                        return Ok(());
                    }

                    let data = buf.split().freeze();
                    if data_tx.send_async(Ok(ConnectData { data })).await.is_err() {
                        error!("data_tx is closed");

                        return Err(anyhow::anyhow!("data_tx is closed"));
                    }
                }
            };

            let _ = futures_util::try_join!(fut1, fut2);
        });

        Ok(Response::new(data_rx.into_stream()))
    }
}

#[async_trait]
impl Hportal for Server {
    type ListenStream = RecvStream<'static, Result<ConnectRequest, Status>>;

    async fn listen(
        &self,
        request: Request<ListenRequest>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        self.handle_listen(request).await
    }

    type EstablishStream = RecvStream<'static, Result<ConnectData, Status>>;

    async fn establish(
        &self,
        request: Request<Streaming<ConnectResultOrData>>,
    ) -> Result<Response<Self::EstablishStream>, Status> {
        self.handle_establish(request).await
    }
}

#[derive(Debug)]
pub struct TcpListenerAddrStream {
    name: String,
    listener: TcpListener,
}

impl Stream for TcpListenerAddrStream {
    type Item = Result<(String, TcpStream), Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let listener = Pin::new(&mut self.listener);

        listener
            .poll_accept(cx)
            .map_ok(|(stream, _)| (self.name.clone(), stream))
            .map(Some)
    }
}
