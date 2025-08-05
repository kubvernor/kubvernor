kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-failopen-pool-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-primary-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-app-backend httproute/httproute-for-secondary-gw --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl delete httproute --namespace gateway-conformance-app-backend httproute-for-primary-gw
kubectl delete httproute --namespace gateway-conformance-app-backend httproute-for-secondary-gw




kubectl patch --namespace gateway-conformance-infra gateway/conformance-primary-gateway --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/conformance-secondary-gateway --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl delete gateway -n gateway-conformance-infra conformance-primary-gateway
kubectl delete gateway -n gateway-conformance-infra conformance-secondary-gateway

kubectl patch --namespace gateway-conformance-app-backend inferencepool/pool-with-invalid-epp --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-app-backend inferencepool/primary-inference-pool --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-app-backend inferencepool/secondary-inference-pool --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl delete inferencepool -n gateway-conformance-app-backend primary-inference-pool
kubectl delete inferencepool -n gateway-conformance-app-backend secondary-inference-pool
kubectl delete inferencepool -n gateway-conformance-app-backend pool-with-invalid-epp


kubectl delete deployment -n gateway-conformance-app-backend primary-app-endpoint-picker
kubectl delete deployment -n gateway-conformance-app-backend secondary-app-endpoint-picker
kubectl delete deployment -n gateway-conformance-app-backend primary-inference-model-server-deployment
kubectl delete deployment -n gateway-conformance-app-backend secondary-inference-model-server-deployment


