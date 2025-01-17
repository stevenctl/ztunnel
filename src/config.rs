use std::net::SocketAddr;

#[derive(Clone)]
pub struct Config {
    pub tls: bool,

    pub window_size: u32,
    pub connection_window_size: u32,
    pub frame_size: u32,

    pub inbound_addr: SocketAddr,
    pub inbound_plaintext_addr: SocketAddr,
    pub outbound_addr: SocketAddr,

    /// The name of the node this ztunnel is running as.
    pub local_node: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            tls: std::env::var("TLS").unwrap_or_else(|_| "".into()) != "off",
            window_size: 4 * 1024 * 1024,
            connection_window_size: 4 * 1024 * 1024,
            frame_size: 1024 * 1024,

            inbound_addr: "[::]:15008".parse().unwrap(),
            inbound_plaintext_addr: "[::]:15006".parse().unwrap(),
            outbound_addr: "[::]:15001".parse().unwrap(),

            local_node: Some(std::env::var("NODE_NAME").unwrap_or_else(|_| "".into()))
                .filter(|s| !s.is_empty()),
        }
    }
}
