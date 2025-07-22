#!/bin/bash
if [[ -z "${CONTROL_PLANE_IP}" ]]; then
  echo "CONTROL_PLANE_IP is undefined, this has to be set and it has to be accessible from Kubernetes/Kind nodes"
  exit 1
fi

export RUST_FILE_LOG=info,kubvernor=debug
export RUST_LOG=info,kubvernor=info
export RUST_TRACE_LOG=info,kubvernor=debug
kubectl apply -f resources/gateway_class.yaml
kubectl apply -f resources/gateway_class_for_inference.yaml
cargo run -- --controller-name "kubvernor.com/proxy-controller" --with-opentelemetry false --envoy-control-plane-hostname $CONTROL_PLANE_IP --envoy-control-plane-port 50051
