#!/bin/bash

PUBLIC_IP=$(curl -H "Metadata-Flavor: Google" http://metadata/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip)

export AMBIENT_PROXY_PUBLIC_HOST_NAME=$PUBLIC_IP
export AMBIENT_PROXY_BIND_ADDRESS=0.0.0.0

cargo run --features=server --release --bin ambient_proxy
