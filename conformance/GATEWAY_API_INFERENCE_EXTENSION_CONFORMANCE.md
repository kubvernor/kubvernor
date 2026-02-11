## **Kubvernor** can be used with different gateways so the conformance results can vary depending on the actual gateway implementation.

1. Set up Kubvernor
```bash
cd scripts/
./deploy_kubvernor.sh
```
2. Set up Gateway API conformance tests
```bash
cd $HOME
git clone --depth 1 --branch v1.1.0 https://github.com/kubernetes-sigs/gateway-api-inference-extension.git
cd gateway-api-inference-extension
cp ../kubvernor/scripts/run_inference_conformance*.sh .
```

3. Testing Kubvernor + Envoy Proxy 
```bash
./run_inference_conformance.sh
```
4. Testing Kubvernor + Orion Proxy 
```bash
./run_inference_conformance_orionproxy.sh
```
5. Testing Kubvernor + Agentgateway 
```bash
./run_inference_conformance_agentgateway.sh
```
