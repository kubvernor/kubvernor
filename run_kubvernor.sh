#!/bin/bash
export RUST_FILE_LOG=info,kubvernor=debug
export RUST_LOG=info,kubvernor=info
export RUST_TRACE_LOG=info,kubvernor=debug
kubectl apply -f resources/gateway_class.yaml
cargo run -- --controller-name "kubvernor.com/proxy-controller" --with-opentelemetry false --envoy-control-plane-hostname 192.168.1.10 --envoy-control-plane-port 50051
