# Kubvernor
Generic Gateway API Manager for Kubernetes

## Running (with minikube)
```
cargo run -- --controller-name "kubvernor.com/proxy-controller"
```

## Deploy resources
```
kubectl apply -f resources/gateway_class.yaml
kubectl apply -f resources/gateway_one.yaml
kubectl apply -f resources/gateway_two.yaml
kubectl apply -f resources/route_one.yaml
kubectl apply -f resources/route_two.yaml

```
