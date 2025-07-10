## [Inference Extension](https://gateway-api-inference-extension.sigs.k8s.io/guides/)

1. Deploy Inference Extension CRDs

```
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/latest/download/manifests.yaml
```

2. Deploy necessary resources - Inference Pool and Endpoint Picker
```
kubectl apply -f resources/inference-resources.yaml
```

3. Deploy Gateway

```
kubectl apply -f ~/Workspace/kubvernor/resources/inference-gateway.yaml
```


3. Deploy HTTP Route
```
k apply -f ~/Workspace/kubvernor/resources/httproute-inference.yaml
```

## Notes/Work

1. Change HTTPRoute to handle different backend types based on a Kind (Service or InferencePool)
1. Change backends_resolver to resolve BackendsRefs with Kind: Inference Pool


