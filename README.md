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



##Conformance hacks
```
https://github.com/envoyproxy/envoy/issues/12383 
echo 1 | sudo tee /proc/sys/user/max_inotify_instances

```


## Conformance tests
```
go test -v ./conformance -run TestConformance -args  --run-test=GatewayHTTPListenerIsolation -debug=true --gateway-class=kubvernor-gateway
go test -v ./conformance -run TestConformance -args  --run-test=GatewayModifyListeners -debug=false --gateway-class=kubvernor-gateway
```
