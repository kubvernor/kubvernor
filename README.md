# Kubvernor
Generic Gateway API Manager for Kubernetes

## Install Kubernetes cluster
A handy way of starting a cluster with Kind is to use [create-cluster.sh](https://github.com/kubernetes-sigs/gateway-api/blob/main/hack/implementations/common/create-cluster.sh) script.

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
go test -v -count=1 -timeout=3h ./conformance --debug -run TestKubvernorGatewayAPIConformance
```


## Conformance hacks
```
https://github.com/envoyproxy/envoy/issues/12383 
echo 1 | sudo tee /proc/sys/user/max_inotify_instances
```
