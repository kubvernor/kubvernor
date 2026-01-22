kubectl patch --namespace gateway-conformance-infra gateway/all-namespaces --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/backend-namespaces --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/http-listener-isolation --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/http-listener-isolation-with-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/same-namespace --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/same-namespace-with-https-listener --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/httproute-listener-hostname-matching --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/gateway-with-one-attached-route --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/gateway-with-two-attached-routes --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/unresolved-gateway-with-one-attached-unresolved-route --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/httproute-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra gateway/httproute-hostname-intersection-all --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'


kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-abc-foo-example-com --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-abc-foo-example-com-with-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-empty-hostname --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-empty-hostname-with-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-wildcard-example-com --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-wildcard-example-com-with-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-wildcard-foo-example-com --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/attaches-to-wildcard-foo-example-com-with-hostname-intersection --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/backend-v1 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/backend-v2 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/backend-v3 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl patch --namespace gateway-conformance-infra grpcroute/backend-v1 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra grpcroute/backend-v2 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra grpcroute/backend-v3 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'

kubectl patch --namespace gateway-conformance-infra httproute/http-route-1 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/http-route-2 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/http-route-3 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/http-route-4 --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/httproute-https-test --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/httproute-https-test-no-hostname --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'


kubectl patch --namespace gateway-conformance-infra httproute/httproute-hostname-intersection-all --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/no-intersecting-hosts --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/specific-host-matches-listener-specific-host --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/specific-host-matches-listener-wildcard-host --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/wildcard-host-matches-listener-specific-host --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/wildcard-host-matches-listener-wildcard-host --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra httproute/matching --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra grpcroute/grpc-header-matching  --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra grpcroute/exact-matching  --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'
kubectl patch --namespace gateway-conformance-infra grpcroute/weighted-backends  --type json --patch='[ { "op": "remove", "path": "/metadata/finalizers" } ]'



kubectl delete namespace gateway-conformance-infra
kubectl delete namespace gateway-conformance-app-backend
kubectl delete gateway-conformance-web-backend




