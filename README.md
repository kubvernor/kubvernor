<p align="center">
<img src="assets/kubvernor-logo-final.png" alt="Logo" width="400"/>
</p>

# Kubvernor 
Generic Gateway API Manager for Kubernetes

>[!CAUTION]
This project is still very unstable and not ready for use in production environments.

Kubvernor is a Rust implementation of Kubernetes Gateway APIs. The aim of the project is to be as generic as possible so Kubvernor could be used to manage/deploy different gateways (Envoy, Nginx, HAProxy, etc.)


## Prerequisites

0. Install Rust, Docker and Kind, Helm ...

1. Deploy your cluster
```bash
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/refs/heads/main/hack/implementations/common/create-cluster.sh | bash
```

2. Install required CRDs for Gateway API
```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.0/standard-install.yaml
```

3. Install Kubvernor CRDs
```bash
kubectl apply -f kubernetes/kubvernor-crds.yaml
```

3. Configure Kubvernor
```bash
kubectl apply -f kubernetes/kubvernor-config.yaml
```
    
> [!NOTE]
> 4. **(Optionally)** install CRDs for Gateway API Inference Extension
>```bash
>kubectl apply -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/download/v1.1.0/manifests.yaml
>```

## Running Inside the Kubernetes Cluster
1. Deploy Kubvernor
```bash
kubectl apply -f kubernetes/kubvernor-deployment.yaml
```

2. All is well if you see a pod in running state
```bash
kubectl get pod -n kubvernor
```

## Run Hello World Gateway API!
1.  Deploy hello world... two gateways, two http routes, one backend
```bash
kubectl apply -f kubernetes/kubvernor-hello-world.yaml
```
2. Check that all is well
```bash
kubectl get gateway
```
3. Make some requests
```bash
curl -vki -H 'Host: service-one.com' http://GATEWAY_ADDRESS:1080/data
```

## Run Hello World Gateway API Inference Extension!
1. Using more or less the steps documented [here](https://gateway-api-inference-extension.sigs.k8s.io/guides/#__tabbed_1_3)

```bash
kubectl apply -f https://raw.githubusercontent.com/kubernetes-sigs/gateway-api-inference-extension/refs/tags/v1.1.0/config/manifests/vllm/sim-deployment.yaml
helm install vllm-llama3-8b-instruct  --set inferencePool.modelServers.matchLabels.app=vllm-llama3-8b-instruct  --set inferencePool.image.pullPolicy=IfNotPresent --set inferenceExtension.image.pullPolicy=IfNotPresent --version v1.1.0  oci://registry.k8s.io/gateway-api-inference-extension/charts/inferencepool
```

2. Deploy Gateway API Inference Extension Routes
```bash
kubectl apply -f kubernetes/kubvernor-hello-inference-world.yaml
```

3. Make some requests
```bash
curl -vki http://GATEWAY_ADDRESS:1080/v1/chat/completions   --header 'Host: www.inference-one.com' -H "Content-Type: application/json"   -d '{"model":"meta-llama/Llama-3.1-8B-Instruct", "messages": [{"role":"user", "content":"What is the story?"}]}'
```


## Cleanup

```bash
kubectl delete -f kubernetes/kubvernor-hello-inference-world.yaml
kubectl delete -f kubernetes/kubvernor-hello-world.yaml
kubectl delete -f kubernetes/kubvernor-deployment.yaml
helm uninstall vllm-llama3-8b-instruct
kubectl delete -f https://raw.githubusercontent.com/kubernetes-sigs/gateway-api-inference-extension/refs/tags/v1.1.0/config/manifests/vllm/sim-deployment.yaml
```


> [!NOTE]
>  ## Running Gateway API Conformance Suite
> 1. Run Gateway API Conformance suite
> 
>```bash
>./run_conformance_tests.sh
>```
> 2. Run Gateway API Inference Extension Conformance tests
>
>```bash
>git clone --depth 1 --branch v1.1.0 https://github.com/kubernetes-sigs/gateway-api-inference-extension.git
>cd gateway-api-inference-extension
>go test -v -count=1 -timeout=3h ./conformance --debug -run TestConformance --report-output="../kubvernor-inference-conformance-output.yaml" --organization=kubvernor --project=kubvernor --url=https://github.com/kubvernor/kubvernor --version=0.1.0  --allow-crds-mismatch
>```


## Gateway API Conformance Reports
[1.2.1](./conformance/kubvernor-conformance-output-1.2.1.yaml)
[1.2.0](./conformance/kubvernor-conformance-output-1.2.0.yaml)
