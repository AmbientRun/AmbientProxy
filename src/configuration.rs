use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    /// The public address of the server (to advertise to the clients)
    ///
    /// Defaults to "localhost"
    pub public_host_name: String,

    /// The address to bind to (IPv4 or IPv6)
    ///
    /// Defautls to "127.0.0.1"
    pub bind_address: String,

    /// The port to bind to for the management interface (used by game servers)
    pub management_port: u16,

    /// The port to bind to for the proxy interface (used by game clients)
    pub proxy_port_first: u16,
    pub proxy_port_last: u16,

    /// The public address of the HTTP interface (for assets downloading)
    ///
    /// Defaults to the same as `public_host_name`
    http_public_host_name: Option<String>,

    /// The port to bind to for the HTTP interface (for assets downloading)
    ///
    /// Defaults to the same as `management_port`
    http_port: Option<u16>,

    /// The timeout for assets downloading (in seconds)
    ///
    /// Defaults to 60 seconds
    pub assets_download_timeout: u32,
}

impl Settings {
    pub fn proxy_port_range(&self) -> RangeInclusive<u16> {
        self.proxy_port_first..=self.proxy_port_last
    }

    pub fn get_http_public_host_name(&self) -> String {
        self.http_public_host_name
            .clone()
            .unwrap_or(self.public_host_name.clone())
    }

    pub fn get_http_port(&self) -> u16 {
        self.http_port.unwrap_or(self.management_port)
    }

    pub fn get_assets_download_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.assets_download_timeout.into())
    }

    pub fn get_bind_addr(&self) -> std::net::IpAddr {
        self.bind_address
            .parse::<std::net::IpAddr>()
            .expect("Failed to parse bind address.")
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            public_host_name: "localhost".to_string(),
            bind_address: "127.0.0.1".to_string(),
            management_port: 7000,
            proxy_port_first: 9000,
            proxy_port_last: 9999,
            http_public_host_name: Default::default(),
            http_port: Default::default(),
            assets_download_timeout: 60,
        }
    }
}

pub fn get_configuration() -> Result<Settings, config::ConfigError> {
    config::Config::builder()
        .add_source(config::Config::try_from(&Settings::default())?)
        .add_source(config::File::with_name("configuration").required(false))
        .add_source(config::Environment::with_prefix("ambient_proxy"))
        .build()?
        .try_deserialize()
}
