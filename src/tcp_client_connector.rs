use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use log::{debug, error};

use crate::address::NetLocation;
use crate::async_stream::AsyncStream;
use crate::config::{ClientConfig, ClientQuicConfig, TcpConfig, Transport};
use crate::quic_stream::QuicStream;
use crate::resolver::{resolve_single_address, Resolver};
use crate::rustls_util::create_client_config;
use crate::socket_util::{new_tcp_socket, new_udp_socket};
use crate::tcp_handler::{TcpClientHandler, TcpClientSetupResult};
use crate::tcp_handler_util::create_tcp_client_handler;
use crate::thread_util::get_num_threads;

const ALWAYS_RESOLVE_HOSTNAMES: bool = false;
const MAX_QUIC_ENDPOINTS: usize = 32;

#[derive(Debug)]
enum TransportConfig {
    Tcp {
        no_delay: bool,
    },
    Quic {
        sni_hostname: Option<String>,
        endpoints: Vec<Arc<quinn::Endpoint>>,
        next_endpoint_index: AtomicU8,
    },
}

#[derive(Debug)]
pub struct TcpClientConnector {
    bind_interface: Option<String>,
    location: NetLocation,
    transport_config: TransportConfig,
    client_handler: Option<Box<dyn TcpClientHandler>>,
}

impl TcpClientConnector {
    pub fn try_from(client_config: ClientConfig) -> Option<Self> {
        let default_sni_hostname = client_config
            .address
            .address()
            .hostname()
            .map(ToString::to_string);

        let transport_config = match client_config.transport {
            Transport::Quic => {
                let ClientQuicConfig {
                    verify,
                    alpn_protocols,
                    sni_hostname,
                } = client_config
                    .quic_settings
                    .unwrap_or_else(ClientQuicConfig::default);

                let sni_hostname = if sni_hostname.is_unspecified() {
                    if default_sni_hostname.is_some() {
                        debug!(
                            "Using default sni hostname for QUIC client connection: {}",
                            default_sni_hostname.as_ref().unwrap()
                        );
                    }
                    default_sni_hostname.clone()
                } else {
                    sni_hostname.into_option()
                };

                let mut quic_client_config =
                    quinn::ClientConfig::new(Arc::new(create_client_config(
                        verify,
                        &alpn_protocols.into_vec(),
                        sni_hostname.is_some(),
                    )));

                let mut transport_config = quinn::TransportConfig::default();

                // From quinn docs:
                // "Applications protocols which forbid remotely-initiated streams should set
                // `max_concurrent_bidi_streams` and `max_concurrent_uni_streams` to zero."
                transport_config
                    .max_concurrent_bidi_streams(0_u32.into())
                    .max_concurrent_uni_streams(0_u8.into())
                    .keep_alive_interval(Some(
                        std::time::Duration::from_secs(15).try_into().unwrap(),
                    ))
                    .max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));

                quic_client_config.transport_config(Arc::new(transport_config));

                let endpoints_len = std::cmp::min(get_num_threads(), MAX_QUIC_ENDPOINTS);

                let mut endpoints = Vec::with_capacity(endpoints_len);

                for _ in 0..endpoints_len {
                    // quinn handles setting the socket to non-blocking.
                    let udp_socket = new_udp_socket(
                        client_config
                            .bind_interface
                            .as_option()
                            .map(ToString::to_string),
                    )
                    .unwrap()
                    .into_std()
                    .unwrap();
                    let mut endpoint = quinn::Endpoint::new(
                        quinn::EndpointConfig::default(),
                        None,
                        udp_socket,
                        quinn::TokioRuntime,
                    )
                    .unwrap();
                    endpoint.set_default_client_config(quic_client_config.clone());
                    endpoints.push(Arc::new(endpoint));
                }

                TransportConfig::Quic {
                    sni_hostname,
                    endpoints,
                    next_endpoint_index: AtomicU8::new(0),
                }
            }
            Transport::Tcp => {
                let TcpConfig { no_delay } = client_config
                    .tcp_settings
                    .unwrap_or_else(TcpConfig::default);
                TransportConfig::Tcp { no_delay }
            }
            _ => {
                panic!("TODO: this is an error, a non-tcp/quic client config was specified for a tcp server");
            }
        };

        Some(Self {
            bind_interface: client_config.bind_interface.clone().into_option(),
            location: client_config.address,
            transport_config,
            client_handler: if client_config.protocol.is_direct() {
                None
            } else {
                Some(create_tcp_client_handler(
                    client_config.protocol,
                    default_sni_hostname,
                ))
            },
        })
    }

    pub fn configure_udp_socket(&self) -> std::io::Result<tokio::net::UdpSocket> {
        let udp_socket = new_udp_socket(self.bind_interface.clone())?;
        Ok(udp_socket)
    }

    pub async fn connect(
        &self,
        server_stream: &mut Box<dyn AsyncStream>,
        mut remote_location: NetLocation,
        resolver: &Arc<dyn Resolver>,
    ) -> std::io::Result<Box<dyn AsyncStream>> {
        let target_addr = if self.client_handler.is_some() {
            // we have a client proxy, connect to the proxy location
            resolve_single_address(resolver, &self.location).await?
        } else {
            // we are directly connecting
            resolve_single_address(resolver, &remote_location).await?
        };

        let client_stream: Box<dyn AsyncStream> = match self.transport_config {
            TransportConfig::Tcp { no_delay } => {
                let tcp_socket =
                    new_tcp_socket(self.bind_interface.clone(), target_addr.is_ipv6()).unwrap();
                let client_stream = tcp_socket.connect(target_addr).await?;
                if no_delay {
                    if let Err(e) = client_stream.set_nodelay(true) {
                        error!("Failed to set TCP no-delay on client socket: {}", e);
                    }
                }
                Box::new(client_stream)
            }
            TransportConfig::Quic {
                ref endpoints,
                ref next_endpoint_index,
                ref sni_hostname,
            } => {
                let domain = match sni_hostname {
                    Some(s) => s,
                    // this is unused since enable_sni is false in create_client_config when we
                    // don't have a hostname.
                    None => self.location.address().hostname().unwrap_or("example.com"),
                };

                let endpoint = if endpoints.len() == 1 {
                    &endpoints[0]
                } else {
                    let endpoint_index =
                        next_endpoint_index.fetch_add(1, Ordering::Relaxed) as usize;
                    &endpoints[endpoint_index % endpoints.len()]
                };

                let conn = endpoint
                    .connect(target_addr, domain)
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to connect to quic endpoint: {}", e),
                        )
                    })?
                    .await
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to connect to quic endpoint: {}", e),
                        )
                    })?;

                let (send, recv) = conn.open_bi().await.map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to open stream to quic endpoint: {}", e),
                    )
                })?;

                Box::new(QuicStream::from(send, recv))
            }
        };

        match self.client_handler {
            Some(ref client_handler) => {
                // TODO: make this configurable
                if ALWAYS_RESOLVE_HOSTNAMES {
                    if remote_location.address().is_hostname() {
                        let socket_addr =
                            resolve_single_address(resolver, &remote_location).await?;
                        remote_location =
                            NetLocation::from_ip_addr(socket_addr.ip(), socket_addr.port());
                    }
                }
                let TcpClientSetupResult { client_stream } = client_handler
                    .setup_client_stream(server_stream, client_stream, remote_location)
                    .await?;

                Ok(client_stream)
            }
            None => Ok(client_stream),
        }
    }
}
