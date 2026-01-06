#!/bin/bash
export RUST_FILE_LOG=info,kubvernor=debug
export RUST_LOG=info,kubvernor=info
export RUST_TRACE_LOG=info,kubvernor=debug
# kubectl apply -f resources/kubvernor_configuration_crd.yaml
# kubectl apply -f resources/gateway_class.yaml
# kubectl apply -f resources/agentgateway_class.yaml
# kubectl apply -f resources/gateway_class_for_inference.yaml
# kubectl apply -f resources/gateway_class_for_inference_test.yaml
cargo run -- --with-config-file config.yaml
