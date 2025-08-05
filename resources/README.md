# Deploying Envoy with kubectl and minikube

## Steps
1. Create config maps for Envoy bootstrap and Envoy XDS
```
kubectl --namespace kubvernor create configmap envoy-cm --from-file ./resources/envoy-bootstrap.yaml
kubectl --namespace kubvernor create configmap envoy-xds --from-file ./resources/rds.yaml --from-file ./resources/cds.yaml --from-file ./resources/lds.yaml
```

2. Create deployment
```
kubectl apply -f ./resources/envoy-deployment.yaml
```

3. Create service and expose it to the outside world
```
kubectl --namespace kubvernor expose deployment envoy --type=LoadBalancer --name=envoy
```
This will create a service but the external IP will be in the pending state.
The service needs to be re-created every time we want to expose a new port in a container

4. Start minikube tunnel
```
minikube tunnel
```

5. Adding a new listener
Will need to re-create a configmap for envoy-xds
```
kubectl --namespace kubvernor delete configmap envoy-xds
kubectl --namespace kubvernor create configmap envoy-xds --from-file ./resources/rds.yaml --from-file ./resources/cds.yaml --from-file ./resources/lds.yaml

```


kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.0/standard-install.yaml


resources/



docker run --rm -p 8080:8080 -p 9901:9901 -it -v ./resources/envoy-inference.yaml:/envoy-config.yaml envoyproxy/envoy:dev -c envoy-config.yaml -l debug --service-node sjksjdksj  --service-cluster djfkdjfkj
