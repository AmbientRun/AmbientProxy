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

    // start management server
    ManagementServer::new(configuration)
        .expect("Failed to create management server.")
        .start()
        .await;

    Ok(())
}
