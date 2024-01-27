use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use bytes::BytesMut;
use futures_util::{stream, Stream, StreamExt, TryStreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tonic::transport::Channel;
use tonic::Streaming;
use tracing::{debug, error, info, instrument};

use crate::auth::Auth;
use crate::proto::connect_result_or_data::Body;
use crate::proto::hportal_client::HportalClient;
use crate::proto::{
    ConnectRequest, ConnectResult, ConnectResultOrData, ListenAddrAndPort, ListenRequest,
};

#[derive(Debug)]
pub struct ListenAddr {
    addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl ListenAddr {
    pub fn new(addr: SocketAddr, remote_addr: SocketAddr) -> Self {
        Self { addr, remote_addr }
    }
}

#[derive(Debug)]
pub struct Client {
    name: String,
    auth: Auth,
    rpc_client: HportalClient<Channel>,
    listen_addrs: HashMap<String, ListenAddr>,
    connect_request_stream: Streaming<ConnectRequest>,
}

impl Client {
    #[instrument(level = "debug", err(Debug))]
    pub async fn new(
        name: String,
        secret: String,
        server_addr: String,
        listen_addrs: HashMap<String, ListenAddr>,
    ) -> anyhow::Result<Self> {
        let channel = Channel::builder(server_addr.parse()?)
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(Duration::from_secs(60))
            .keep_alive_timeout(Duration::from_secs(180))
            .connect()
            .await?;

        info!(server_addr, "connect server done");

        let mut rpc_client = HportalClient::new(channel);
        let auth = Auth::new(secret)?;

        let connect_request_stream =
            Self::listen(&mut rpc_client, &listen_addrs, &name, auth.generate_token()).await?;

        Ok(Self {
            name,
            auth,
            rpc_client,
            listen_addrs,
            connect_request_stream,
        })
    }

    #[instrument(level = "debug", err(Debug))]
    async fn listen(
        rpc_client: &mut HportalClient<Channel>,
        listen_addrs: &HashMap<String, ListenAddr>,
        name: &str,
        token: String,
    ) -> anyhow::Result<Streaming<ConnectRequest>> {
        let listen_addr_and_ports = listen_addrs
            .iter()
            .map(|(name, addr)| ListenAddrAndPort {
                name: name.clone(),
                remote_addr: addr.remote_addr.ip().to_string(),
                remote_port: addr.remote_addr.port() as _,
            })
            .collect();

        let request = ListenRequest {
            client_name: name.to_string(),
            token,
            listen_addr_and_ports,
        };

        let connect_req_stream = rpc_client.listen(request).await?.into_inner();

        Ok(connect_req_stream)
    }

    async fn re_listen(&mut self) {
        loop {
            match Self::listen(
                &mut self.rpc_client,
                &self.listen_addrs,
                &self.name,
                self.auth.generate_token(),
            )
            .await
            {
                Err(err) => {
                    error!(%err, "re listen failed");

                    time::sleep(Duration::from_secs(3)).await;
                }

                Ok(connect_request_stream) => {
                    self.connect_request_stream = connect_request_stream;

                    return;
                }
            }
        }
    }

    pub async fn run(&mut self) {
        loop {
            let res = match self.connect_request_stream.next().await {
                None => {
                    error!("accept connect request failed, stream is closed");

                    self.re_listen().await;

                    continue;
                }

                Some(res) => res,
            };

            let connect_request = match res {
                Err(err) => {
                    error!(%err, "accept connect request failed");

                    self.re_listen().await;

                    continue;
                }

                Ok(connect_request) => connect_request,
            };

            self.handle_connect_request(connect_request);
        }
    }

    #[instrument(level = "debug")]
    fn handle_connect_request(&mut self, ConnectRequest { name, id }: ConnectRequest) {
        let mut rpc_client = self.rpc_client.clone();
        let listen_addr = match self.listen_addrs.get(&name) {
            None => {
                error!(name, "connect request name not exist");

                tokio::spawn(async move {
                    let _ = rpc_client
                        .establish(stream::iter([ConnectResultOrData {
                            body: Some(Body::Result(ConnectResult {
                                id,
                                fail_reason: Some("connect request name not exist".to_string()),
                            })),
                        }]))
                        .await;
                });

                return;
            }

            Some(listen_addr) => listen_addr,
        };

        debug!(name, ?listen_addr, "get listen addr done");

        let local_addr = listen_addr.addr;
        tokio::spawn(async move {
            let mut tcp_stream = match TcpStream::connect(local_addr).await {
                Err(err) => {
                    error!(%err, %local_addr, "connect local addr failed");

                    let _ = rpc_client
                        .establish(stream::iter([ConnectResultOrData {
                            body: Some(Body::Result(ConnectResult {
                                id,
                                fail_reason: Some(err.to_string()),
                            })),
                        }]))
                        .await;

                    return;
                }

                Ok(tcp_stream) => tcp_stream,
            };

            debug!(%local_addr, "connect local addr done");

            let (tx, rx) = flume::bounded(1);

            tx.send(ConnectResultOrData {
                body: Some(Body::Result(ConnectResult {
                    id,
                    fail_reason: None,
                })),
            })
            .unwrap();

            let connect_data_stream = rx.into_stream();
            let mut resp_stream = match rpc_client
                .establish(assert_stream(connect_data_stream))
                .await
            {
                Err(err) => {
                    error!(%err, "establish failed");

                    return;
                }

                Ok(resp) => resp.into_inner(),
            };

            let (mut tcp_read, mut tcp_write) = tcp_stream.split();
            let fut1 = async {
                let mut buf = BytesMut::with_capacity(16 * 1024);
                loop {
                    if buf.capacity() < 8 * 1204 {
                        buf.reserve(8 * 1204);
                    }

                    let n = tcp_read
                        .read_buf(&mut buf)
                        .await
                        .with_context(|| "read tcp failed")?;
                    if n == 0 {
                        drop(tx);

                        debug!("tcp read is closed");

                        return Ok(());
                    }

                    let data = buf.split().freeze();

                    if tx
                        .send_async(ConnectResultOrData {
                            body: Some(Body::Data(data)),
                        })
                        .await
                        .is_err()
                    {
                        error!("connect_data_rx is closed");

                        return Err(anyhow::anyhow!("connect_data_rx is closed"));
                    }
                }
            };

            let fut2 = async {
                while let Some(data) = resp_stream
                    .try_next()
                    .await
                    .with_context(|| "resp_stream is broken")?
                {
                    tcp_write
                        .write_all(&data.data)
                        .await
                        .with_context(|| "tcp write failed")?;
                }

                debug!("resp stream is closed");

                let _ = tcp_write.shutdown().await;

                Ok::<_, anyhow::Error>(())
            };

            let _ = futures_util::try_join!(fut1, fut2);
        });
    }
}

fn assert_stream<S: Stream + Send + 'static>(s: S) -> impl Stream<Item = S::Item> + Send + 'static {
    s
}
