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
curl -vki 172.18.255.200:2080/v1/completions -H 'Content-Type: application/json' -d '{"model": "food-review", "prompt":"Write as if you were a critic: San Francisco", "max_tokens":100, "temperature":0}'

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




    apply.go:301: 2025-07-15T20:42:26.723832174+01:00: Deleting gateway-conformance-app-backend Namespace
    apply.go:301: 2025-07-15T20:42:27.085234597+01:00: Deleting gateway-conformance-infra Namespace
--- FAIL: TestConformance (666.00s)
    --- PASS: TestConformance/EppUnAvailableFailOpen (28.14s)
        --- PASS: TestConformance/EppUnAvailableFailOpen/Phase_1:_Verify_baseline_connectivity_with_EPP_available (25.10s)
        --- PASS: TestConformance/EppUnAvailableFailOpen/Phase_2:_Verify_fail-open_behavior_after_EPP_becomes_unavailable (0.31s)
    --- FAIL: TestConformance/GatewayFollowingEPPRouting (79.17s)
        --- FAIL: TestConformance/GatewayFollowingEPPRouting/should_route_traffic_to_a_single_designated_pod (0.05s)
        --- FAIL: TestConformance/GatewayFollowingEPPRouting/should_route_traffic_to_two_designated_pods (0.04s)
        --- PASS: TestConformance/GatewayFollowingEPPRouting/should_route_traffic_to_all_available_pods (0.04s)
    --- PASS: TestConformance/HTTPRouteInvalidInferencePoolRef (2.36s)
        --- PASS: TestConformance/HTTPRouteInvalidInferencePoolRef/HTTPRoute_should_have_Accepted=True_and_ResolvedRefs=False_for_non-existent_InferencePool (2.02s)
    --- PASS: TestConformance/HTTPRouteMultipleGatewaysDifferentPools (1.32s)
        --- PASS: TestConformance/HTTPRouteMultipleGatewaysDifferentPools/Primary_HTTPRoute,_InferencePool,_and_Gateway_path:_verify_status_and_traffic (1.11s)
        --- PASS: TestConformance/HTTPRouteMultipleGatewaysDifferentPools/Secondary_HTTPRoute,_InferencePool,_and_Gateway_path:_verify_status_and_traffic (0.04s)
    --- PASS: TestConformance/InferencePoolAccepted (0.69s)
        --- PASS: TestConformance/InferencePoolAccepted/InferencePool_should_have_Accepted_condition_set_to_True (0.05s)
    --- PASS: TestConformance/InferencePoolHTTPRoutePortValidation (2.79s)
        --- PASS: TestConformance/InferencePoolHTTPRoutePortValidation/Scenario_1:_HTTPRoute_backendRef_to_InferencePool_with_Port_Unspecified (2.06s)
        --- PASS: TestConformance/InferencePoolHTTPRoutePortValidation/Scenario_2:_HTTPRoute_backendRef_to_InferencePool_with_Port_Specified_and_Matching (0.01s)
        --- PASS: TestConformance/InferencePoolHTTPRoutePortValidation/Scenario_3:_HTTPRoute_backendRef_to_InferencePool_with_Port_Specified_and_Non-Matching._Request_still_passing_because_HTTP_Port_is_ignored_when_inferencePool_is_backendRef (0.01s)
    --- PASS: TestConformance/InferencePoolInvalidEPPService (2.71s)
        --- PASS: TestConformance/InferencePoolInvalidEPPService/InferecePool_has_a_ResolvedRefs_Condition_with_status_False (0.00s)
        --- PASS: TestConformance/InferencePoolInvalidEPPService/Request_to_a_route_with_an_invalid_backend_reference_receives_a_500_response (0.00s)
    --- PASS: TestConformance/HTTPRouteMultipleRulesDifferentPools (3.30s)
        --- PASS: TestConformance/HTTPRouteMultipleRulesDifferentPools/Wait_for_resources_to_be_accepted (3.09s)
        --- PASS: TestConformance/HTTPRouteMultipleRulesDifferentPools/Traffic_should_be_routed_to_the_correct_pool_based_on_path (0.01s)
            --- PASS: TestConformance/HTTPRouteMultipleRulesDifferentPools/Traffic_should_be_routed_to_the_correct_pool_based_on_path/request_to_primary_pool (0.01s)
            --- PASS: TestConformance/HTTPRouteMultipleRulesDifferentPools/Traffic_should_be_routed_to_the_correct_pool_based_on_path/request_to_secondary_pool (0.00s)
    --- FAIL: TestConformance/InferencePoolResolvedRefsCondition (515.80s)
        --- PASS: TestConformance/InferencePoolResolvedRefsCondition/InferencePool_should_show_Accepted:True_by_parents_and_be_routable_via_multiple_HTTPRoutes (0.01s)
        --- FAIL: TestConformance/InferencePoolResolvedRefsCondition/Delete_httproute-for-primary-gw_and_verify_InferencePool_status_and_routing_via_secondary_gw (205.10s)
        --- FAIL: TestConformance/InferencePoolResolvedRefsCondition/Delete_httproute-for-secondary-gw_and_verify_InferencePool_has_no_parent_statuses_and_is_not_routable (305.03s)


go test -v -count=1 -timeout=3h ./conformance --debug -run TestConformance/InferencePoolResolvedRefsCondition  --allow-crds-mismatch