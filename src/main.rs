use std::net::{SocketAddr, TcpListener};

use ambient_proxy::{
    server::{start_http_interface, ManagementServer},
    telemetry::{get_subscriber, init_subscriber},
};
use tokio::runtime::Handle;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up tracing and logging
    let subscriber = get_subscriber("ambient-proxy".into(), "info".into(), std::io::stdout);
    init_subscriber(subscriber);

    // start management server
    let server = ManagementServer::new(SocketAddr::from(([0, 0, 0, 0], 7000)))?;

    // start http listener for asset downloading
    // FIXME: move to a startup-alike mod and factor out addr to configuration
    let http_addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    start_http_interface(Handle::current(), TcpListener::bind(http_addr)?);

    tokio::spawn(async move {
        server.start().await;
    });

    Ok(tokio::signal::ctrl_c().await?)
}
