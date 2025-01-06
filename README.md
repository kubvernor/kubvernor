# Kubvernor
Generic Gateway API Manager for Kubernetes

## Install CRDs
```
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.0/standard-install.yaml
```

## Running (with minikube)
```
export RUST_FILE_LOG=info,kubvernor=debug
export RUST_LOG=info,kubvernor=info
export RUST_TRACE_LOG=info,kubvernor=debug
kubectl apply -f resources/gateway_class.yaml
cargo run -- --controller-name "kubvernor.com/proxy-controller" --with-opentelemetry false
```

## Run conformance suite
```
cd conformance
go test -v -timeout=3h -v ./conformance -run TestKubvernorGatewayAPIConformance
```


## Conformance hacks
```
https://github.com/envoyproxy/envoy/issues/12383 
echo 1 | sudo tee /proc/sys/user/max_inotify_instances
```
