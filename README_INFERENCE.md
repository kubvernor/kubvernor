## [Inference Extension](https://gateway-api-inference-extension.sigs.k8s.io/guides/)

1. Deploy Inference Extension CRDs

```
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/latest/download/manifests.yaml
```

2. Deploy necessary resources - Inference Pool and Endpoint Picker
```
kubectl apply -f resources/inference-resources.yaml
```

3. Deploy Inference Model Server
```
# kubectl apply -f resources/inference-model-server-cpu.yaml
kubectl apply -f resources/inference-model-server-sim.yaml
```

4. Deploy Model
```
kubectl apply -f resources/inference-model.yaml
```

5. Deploy Gateway

```
kubectl apply -f resources/inference-gateway.yaml
```


6. Deploy HTTP Route
```
kubectl apply -f resources/inference-httproute.yaml
```

7. Test
```
curl -vki 172.18.255.200:2080/v1/chat/completions -d '{ "model": "meta-llama/Llama-3.1-8B-Instruct", "messages": [{"role":"developer", "content":"hello"}]}'
curl -vki 192.168.1.10:3000/v1/completions -H 'Content-Type: application/json' -d '{"model": "food-review", "prompt":"Write as if you were a critic: San Francisco", "max_tokens":100, "temperature":0}'

```

## Notes/Work

1. Change HTTPRoute to handle different backend types based on a Kind (Service or InferencePool)
1. Change backends_resolver to resolve BackendsRefs with Kind: Inference Pool
1. getting 503, no healthy upstream, not sure if we call to the ext service? will need to find the actual endpoint for epp ?
1. building and running local epp
```
dawid@dawid-Alienware-Aurora-R6:~/Workspace/gateway-api-inference-extension/cmd/epp$ go build
dawid@dawid-Alienware-Aurora-R6:~/Workspace/gateway-api-inference-extension/cmd/epp$ ./epp --poolName vllm-llama3-8b-instruct --poolNamespace default -grpcPort 9002 -grpcHealthPort 9003 -v 6 - --zap-encoder json  -zap-devel --secureServing="false"
```

docker run --rm -p 3000:3000 -p 9901:9901 -it -v ./resources/envoy-inference.yaml:/envoy-config.yaml envoyproxy/envoy:dev -c envoy-config.yaml -l trace --service-node sjksjdksj  --service-cluster djfkdjfkj