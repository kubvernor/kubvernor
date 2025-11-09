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
   curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/refs/heads/main/hack/implementations/common/create-cluster.sh | sh

   ```


3. Install required CRDs for Gateway API
    ```bash
    kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml
    ```



4. Compile and run Kubvernor

    ```bash
   export CONTROL_PLANE_IP=<IP>
   ./run_kubvernor.sh
   ```

5. Run Gateway API Conformance suite

    ```bash
    ./run_conformance_tests.sh
    ```

> [!NOTE]
> Experimenting with Gateway API Inference Extension
>
> 6. **(Optionally)** install CRDs for Gateway API Inference Extension
>    ```bash
>    kubectl apply -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/download/v1.1.0/manifests.yaml
>    ```
> 7. Run Gateway API Inference Extension Conformance tests
>
>    ```bash
>    git clone --depth 1 --branch v1.1.0 https://github.com/kubernetes-sigs/gateway-api-inference-extension.git
>    cd gateway-api-inference-extension
>    go test -v -count=1 -timeout=3h ./conformance --debug -run TestConformance --report-output="../kubvernor-inference-conformance-output.yaml" --organization=kubvernor --project=kubvernor --url=https://github.com/kubvernor/kubvernor --version=0.1.0  --allow-crds-mismatch
>    ```

## Gateway API Conformance reports
[1.2.1](./conformance/kubvernor-conformance-output-1.2.1.yaml)
[1.2.0](./conformance/kubvernor-conformance-output-1.2.0.yaml)
