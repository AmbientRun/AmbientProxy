use std::net::SocketAddr;

use ambient_proxy::{
    server::ManagementServer,
    telemetry::{get_subscriber, init_subscriber},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up tracing and logging
    let subscriber = get_subscriber("ambient-proxy".into(), "info".into(), std::io::stdout);
    init_subscriber(subscriber);

    // start management server
    ManagementServer::new(
        SocketAddr::from(([0, 0, 0, 0], 7000)),
        SocketAddr::from(([0, 0, 0, 0], 8080)),
    )?
    .start()
    .await;

    Ok(())
}
