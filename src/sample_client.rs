use std::net::SocketAddr;

use ambient_proxy::client::Client;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = Client::connect(SocketAddr::from(([127, 0, 0, 1], 7000))).await?;
    client.dump().await;
    Ok(tokio::signal::ctrl_c().await?)
}
