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



##Conformanc hacks
```
https://github.com/envoyproxy/envoy/issues/12383 
echo 1 | sudo tee /proc/sys/user/max_inotify_instances

```
