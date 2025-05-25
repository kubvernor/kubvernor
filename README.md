# Kubvernor
Generic Gateway API Manager for Kubernetes

>[!CAUTION]
This project is still very unstable and not ready for use in production environments. 

Kubvernor is a Rust implementation of Kubernetes Gateway APIs. The aim of the project is to be as generic as possible so Kubvernor could be used to manage/deploy different gateways (Envoy, Nginx, HAProxy, etc.)


## Running 

0. Install Rust, Docker and Kind

1. Clone the Kubvernor GitHub repository
   ```bash
   git clone https://github.com/kubvernor/kubvernor && cd kubvernor
   ```

2. Deploy your cluster
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://github.com/kubernetes-sigs/gateway-api/blob/main/hack/implementations/common/create-cluster.sh | sh

   ```

3. Install required CRDs
    ```bash
    kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml
    ```


4. Compile and run Kubvernor
    ```bash         
   export CONTROL_PLANE_IP=<IP>
   ./run_kubvernor.sh 
   
   ```
5. Run conformance suite
    ```bash
    ./run_conformance_tests.sh
    ```


## Conformance reports
[1.2.1](./conformance/kubvernor-conformance-output-1.2.1.yaml)  
[1.2.0](./conformance/kubvernor-conformance-output-1.2.0.yaml)

