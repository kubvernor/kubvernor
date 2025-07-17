kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-failopen-pool-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-primary-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-secondary-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl delete httproute --namespace gateway-conformance-app-backend httproute-for-primary-gw
kubectl delete httproute --namespace gateway-conformance-app-backend httproute-for-secondary-gw
kubectl delete inferencepool secondary-inference-pool  -n gateway-conformance-app-backend 
kubectl delete inferencepool primary-inference-pool  -n gateway-conformance-app-backend



kubectl patch --namespace gateway-conformance-infra gateway/conformance-primary-gateway --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/conformance-secondary-gateway --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl delete gateway -n gateway-conformance-infra conformance-primary-gateway
kubectl delete gateway -n gateway-conformance-infra conformance-secondary-gateway