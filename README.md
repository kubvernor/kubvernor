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
go test -v -count=1 -timeout=3h ./conformance --debug -run TestKubvernorGatewayAPIConformance
```

At the moment this is a pretty long process (roughly 1h). This is due to setting the isolation time between the tests to two minutes. 
The reason for that is that our implementation is using Kubernetes config maps to configure Envoy. Which means that there is no need for starting up an additional server with Envoy control plane... 
Unfortunately, propagating updates via config maps can take much longer than pushing updates over gRPC due to kubelet synchronization time which can be anywhere from 30-60secs.


## Conformance hacks
```
https://github.com/envoyproxy/envoy/issues/12383 
echo 1 | sudo tee /proc/sys/user/max_inotify_instances
```
