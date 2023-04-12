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
