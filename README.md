# Kubvernor
Generic Gateway API Manager for Kubernetes

## Install Kubernetes cluster
A handy way of starting a cluster with Kind is to use [create-cluster.sh](https://github.com/kubernetes-sigs/gateway-api/blob/main/hack/implementations/common/create-cluster.sh) script.

## Install CRDs
```
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml
```

## Running
```
export RUST_FILE_LOG=info,kubvernor=debug
export RUST_LOG=info,kubvernor=info
export RUST_TRACE_LOG=info,kubvernor=debug
kubectl apply -f resources/gateway_class.yaml
cargo run -- --controller-name "kubvernor.com/proxy-controller" --with-opentelemetry false --envoy-control-plane-hostname <ip or hostname which can be resolved from the pods, not 127.0.0.1 and not 0.0.0.0>>  --envoy-control-plane-port 50051
```



## Run conformance suite
```
cd conformance
go test -v -count=1 -timeout=3h ./conformance --debug -run TestKubvernorGatewayAPIConformance
```

```
cd conformance
go test -v -count=1 -timeout=3h ./conformance --debug -run TestKubvernorGatewayAPIConformanceExperimental --report-output="../kubernor-conformance-output.yaml" --organization=kubvernor --project=kubvernor --url=https://github.com/kubvernor --version=latest  --contact=nowakd@gmail.com
```

## Conformance report
[1.2.1](./conformance/kubvernor-conformance-output-1.2.1.yaml)  
[1.2.0](./conformance/kubvernor-conformance-output-1.2.0.yaml)

