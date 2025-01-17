use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::proxy::Error;
use crate::workload::{Protocol, Workload, WorkloadInformation};
use crate::{socket, tls};

pub struct Outbound {
    cfg: Config,
    workloads: Arc<Mutex<WorkloadInformation>>,
    listener: TcpListener,
}

impl Outbound {
    pub async fn new(
        cfg: Config,
        workloads: Arc<Mutex<WorkloadInformation>>,
    ) -> Result<Outbound, Error> {
        let listener: TcpListener = TcpListener::bind(cfg.outbound_addr)
            .await
            .map_err(Error::Bind)?;
        match socket::set_transparent(&listener) {
            Err(_e) => info!("running without transparent mode"),
            _ => info!("running with transparent mode"),
        };

        Ok(Outbound {
            cfg,
            workloads,
            listener,
        })
    }

    pub(super) async fn run(self) {
        let addr = self.listener.local_addr().unwrap();
        info!("outbound listener established {}", addr);

        loop {
            // Asynchronously wait for an inbound socket.
            let socket = self.listener.accept().await;
            match socket {
                Ok((stream, remote)) => {
                    info!("accepted outbound connection from {}", remote);
                    let cfg = self.cfg.clone();
                    let oc = OutboundConnection {
                        workloads: self.workloads.clone(),
                        cfg,
                    };
                    tokio::spawn(async move {
                        let res = oc.proxy(stream).await;
                        match res {
                            Ok(_) => info!("outbound proxy complete"),
                            Err(ref e) => warn!("outbound proxy failed: {:?}", e),
                        };
                    });
                }
                Err(e) => error!("Failed TCP handshake {}", e),
            }
        }
    }
}

struct OutboundConnection {
    workloads: Arc<Mutex<WorkloadInformation>>,
    // TODO: Config may be excessively large, maybe we store a scoped OutboundConfig intended for cloning.
    cfg: Config,
}

impl OutboundConnection {
    async fn proxy(&self, mut stream: TcpStream) -> Result<(), Error> {
        // For now we only support IPv4 but we are binding to IPv6 address; convert everything to IPv4
        let remote_addr = match stream.peer_addr().expect("must receive peer addr").ip() {
            IpAddr::V4(i) => IpAddr::V4(i),
            IpAddr::V6(i) => IpAddr::V4(i.to_ipv4().unwrap()),
        };
        let orig = socket::orig_dst_addr(&stream).expect("must have original dst enabled");
        debug!("request from {} to {}", remote_addr, orig);
        let req = self.build_request(remote_addr, orig);
        match req.protocol {
            Protocol::Hbone => {
                info!(
                    "Proxying to {} using HBONE via {} type {:#?}",
                    req.destination, req.gateway, req.request_type
                );

                // Using the raw connection API, instead of client, is a bit annoying, but the only reasonable
                // way to work around https://github.com/hyperium/hyper/issues/2863
                // Eventually we will need to implement our own smarter pooling, TLS handshaking, etc anyways.
                let mut builder = hyper::client::conn::Builder::new();
                let builder = builder
                    .http2_only(true)
                    .http2_initial_stream_window_size(self.cfg.window_size)
                    .http2_max_frame_size(self.cfg.frame_size)
                    .http2_initial_connection_window_size(self.cfg.connection_window_size);

                let request = hyper::Request::builder()
                    .uri(&req.destination.to_string())
                    .method(hyper::Method::CONNECT)
                    .version(hyper::Version::HTTP_2)
                    .body(hyper::Body::empty())
                    .unwrap();

                let mut request_sender = if self.cfg.tls {
                    let tcp_stream = TcpStream::connect(req.gateway).await?;
                    let tls_stream = connect_tls(tcp_stream).await?;
                    let (request_sender, connection) = builder
                        .handshake(tls_stream)
                        .await
                        .map_err(Error::HttpHandshake)?;
                    // spawn a task to poll the connection and drive the HTTP state
                    tokio::spawn(async move {
                        if let Err(e) = connection.await {
                            error!("Error in HBONE connection handshake: {:?}", e);
                        }
                    });
                    request_sender
                } else {
                    let tcp_stream = TcpStream::connect(req.gateway).await?;
                    let (request_sender, connection) = builder
                        .handshake::<TcpStream, hyper::Body>(tcp_stream)
                        .await?;
                    // spawn a task to poll the connection and drive the HTTP state
                    tokio::spawn(async move {
                        if let Err(e) = connection.await {
                            error!("Error in connection: {}", e);
                        }
                    });
                    request_sender
                };

                let response = request_sender.send_request(request).await?;

                let code = response.status();
                match hyper::upgrade::on(response).await {
                    Ok(mut upgraded) => {
                        super::copy_hbone("hbone client", &mut upgraded, &mut stream)
                            .await
                            .expect("hbone client copy");
                    }
                    Err(e) => eprintln!("upgrade error: {}, {}", e, code),
                }
                info!("request complete");
                Ok(())
            }
            Protocol::Tcp => {
                info!(
                    "Proxying to {} using TCP via {} type {:?}",
                    req.destination, req.gateway, req.request_type
                );
                let mut outbound = TcpStream::connect(req.gateway).await?;

                let (mut ri, mut wi) = stream.split();
                let (mut ro, mut wo) = outbound.split();

                let client_to_server = async {
                    tokio::io::copy(&mut ri, &mut wo).await?;
                    wo.shutdown().await
                };

                let server_to_client = async {
                    tokio::io::copy(&mut ro, &mut wi).await?;
                    wi.shutdown().await
                };

                tokio::try_join!(client_to_server, server_to_client)?;

                Ok(())
            }
        }
    }

    fn build_request(&self, downstream: IpAddr, target: SocketAddr) -> Request {
        let (source_workload, us, is_vip) = {
            let wi = self.workloads.lock().unwrap();
            let source_workload = wi
                .find_workload(&downstream)
                .expect("todo: source must be found")
                .clone();

            let (us, is_vip) = wi.find_upstream(target);
            (source_workload, us, is_vip)
        };
        let mut req = Request {
            protocol: us.workload.protocol,
            _source: source_workload.clone(), // TODO drop clone
            destination: SocketAddr::from((us.workload.workload_ip, us.port)),
            gateway: us
                .workload
                .gateway_ip
                .expect("todo: refactor gateway ip handling"),
            direction: Direction::Outbound, // TODO set this
            request_type: RequestType::Direct,
        };
        if source_workload.remote_proxy.is_some() {
            // Source has a remote proxy. We should delegate everything to that proxy - do not even resolve VIP.
            // TODO: add client skipping
            req.request_type = RequestType::ToClientWaypoint;
            // Let the client remote know we are on the outbound path. The remote proxy should strictly
            // validate the identity when we declare this
            req.direction = Direction::Outbound;
            // Load balancing decision is deferred to remote proxy
            req.destination = target;
            // Send to the remote proxy
            req.gateway = SocketAddr::from((source_workload.remote_proxy.unwrap(), 15001));
            // Always use HBONE here
            req.protocol = Protocol::Hbone;
        } else if us.workload.remote_proxy.is_some() {
            // TODO: even in this case, we are picking a single upstream pod and deciding if it has a remote proxy.
            // Typically this is all or nothing, but if not we should probably send to remote proxy if *any* upstream has one.
            if is_vip {
                // Use the original VIP, not translated
                req.destination = target
            }
            req.request_type = RequestType::ToServerWaypoint;
            // Always use HBONE here
            req.protocol = Protocol::Hbone;
            // Let the client remote know we are on the inbound path.
            req.direction = Direction::Inbound;
            req.gateway = SocketAddr::from((us.workload.remote_proxy.unwrap(), 15006));
        } else if !us.workload.node.is_empty()
            && self.cfg.local_node == Some(us.workload.node)
            && req.protocol == Protocol::Hbone
        {
            // Sending to a node on the same node (ourselves). Requests from the node proxy are not captured,
            // so we need to explicitly send it to ourselves.
            // In the future this could be optimized to avoid a full network traversal.
            req.request_type = RequestType::DirectLocal;
            req.gateway = "127.0.0.1:15008".parse().unwrap();
        } else if us.workload.name.is_empty() {
            req.request_type = RequestType::Passthrough;
        } else {
            req.request_type = RequestType::Direct;
        }
        req
    }
}

#[derive(Debug)]
struct Request {
    protocol: Protocol,
    direction: Direction,
    _source: Workload,
    destination: SocketAddr,
    gateway: SocketAddr,
    request_type: RequestType,
}

#[derive(Debug)]
enum Direction {
    Inbound,
    Outbound,
}

#[derive(Debug)]
enum RequestType {
    ToClientWaypoint,
    ToServerWaypoint,
    Direct,
    DirectLocal,
    Passthrough,
}

async fn connect_tls(
    stream: TcpStream,
) -> Result<tokio_boring::SslStream<TcpStream>, tokio_boring::HandshakeError<TcpStream>> {
    let conn = tls::test_certs().connector();
    let mut cfg = conn.unwrap().configure().unwrap();
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    let addr = stream.local_addr();
    cfg.set_verify_callback(boring::ssl::SslVerifyMode::PEER, move |_, x509| {
        info!("TLS callback for {:?}: {:?}", addr, x509.error());
        true
    });
    tokio_boring::connect(cfg, "", stream).await
}
