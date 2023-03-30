use std::net::IpAddr;

use ambient_proxy::{
    configuration::get_configuration,
    server::ManagementServer,
    telemetry::{get_subscriber, init_subscriber},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up tracing and logging
    let subscriber = get_subscriber("ambient-proxy".into(), "info".into(), std::io::stdout);
    init_subscriber(subscriber);

    // read configuration
    let configuration = get_configuration().expect("Failed to read configuration.");
    tracing::debug!("Configuration: {:?}", configuration);
    let bind_addr = configuration
        .bind_address
        .parse::<IpAddr>()
        .expect("Failed to parse bind address.");

    // start management server
    ManagementServer::new(
        bind_addr,
        configuration.management_port,
        configuration.get_http_port(),
        configuration.proxy_port_range(),
        configuration.public_host_name.clone(),
        configuration.get_http_public_host_name(),
    )
    .expect("Failed to create management server.")
    .start()
    .await;

    Ok(())
}
