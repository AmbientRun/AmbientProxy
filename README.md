# AmbientProxy
NAT traversal proxy for Ambient game engine.

## Protocol

Communication between Ambient server and the proxy uses QUIC through `quinn` library. Messages are defined in [protocol.rs](src/protocol.rs).

Proxy server requests assets and notifies about opened connections, streams and received datagrams. Ambient server can open streams to players, send datagrams to them and store assets on proxy to make them available via proxy's HTTP interface.

After allocation, proxy starts listening to client connections on a separate port. Ambient server is notified about each connection. Each stream opened on that connection results in opening an equivalent stream to Ambient server, the stream is prefixed with `ServerStreamHeader` for identification and then simply copied. Similarly all datagrams received from players/clients are prefixed with `DatagramInfo` and transmitted to Ambient server.

Communication between proxy and clients (on allocated endpoint) uses the same protocols as direct connection between Ambient server (`ambient run` or `ambient serve`) and client (`ambient join`).

## Assets

Each allocation is assigned an id (UUID v4). This determines the base URL for assets retrieval, the URL is passed in `ServerMessage::Allocation` to the Ambient server and then passed to clients connecting to the proxy.

When proxy receives a GET request for an asset that is not already cached, it will use the internal protocol to request the asset from Ambient server. Ambient server can also pre-cache assets on the proxy.

# Development

Server code is behind `server` feature flag for easy use of this crate as a client library.

## Testing proxy server

Proxy server supports overriding default configuration via environment variables. For testing it's recommended to use self signed certificates (included in the repository, configuration defaults to using them) and `bunyan` log format:

```sh
RUST_LOG=info,ambient_proxy=trace LOG_FORMAT=bunyan cargo run --features server | bunyan
```

Running server can be used by Ambient by setting CA certificate override and providing `--proxy` argument with proxy server address (note that for tls validation proxy needs to have a name, the self signed certificates use `localhost`). For example:

```sh
AMBIENT_PROXY_TEST_CA_CERT=../AmbientProxy/self-signed-certs/ca.der cargo run -- run --proxy localhost:7000 guest/rust/examples/games/minigolf
```

Clients should be able to join the endpoint allocated by the proxy but they have to override CA certificate, for example:

```sh
cargo run -- join --ca ../AmbientProxy/self-signed-certs/ca.der localhost:9529
```
