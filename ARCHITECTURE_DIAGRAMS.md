# Kubvernor Architecture Diagrams

## 1. High-Level Component Architecture

```mermaid
graph TB
    subgraph "Kubernetes API Server"
        K8S_API[Kubernetes API]
    end

    subgraph "Controllers Layer"
        GWC[GatewayClass Controller]
        GW[Gateway Controller]
        HTTP[HTTPRoute Controller]
        GRPC[GRPCRoute Controller]
        IP[InferencePool Controller]
    end

    subgraph "State Management"
        STATE[(In-Memory State<br/>Mutex-Protected)]
    end

    subgraph "Services Layer"
        RVS[Reference Validator Service]
        GDS[Gateway Deployer Service]
        PATCH_GWC[GatewayClass Patcher]
        PATCH_GW[Gateway Patcher]
        PATCH_HTTP[HTTPRoute Patcher]
        PATCH_GRPC[GRPCRoute Patcher]
        PATCH_IP[InferencePool Patcher]
    end

    subgraph "Backends Layer"
        ENVOY_XDS[Envoy XDS Backend<br/>gRPC Control Plane]
        ENVOY_CM[Envoy ConfigMap Backend]
        AGENT_GW[AgentGateway Backend]
    end

    subgraph "External Systems"
        ENVOY[Envoy Proxy]
        K8S_WORKLOADS[Kubernetes Workloads]
    end

    K8S_API -->|Watch Events| GWC
    K8S_API -->|Watch Events| GW
    K8S_API -->|Watch Events| HTTP
    K8S_API -->|Watch Events| GRPC
    K8S_API -->|Watch Events| IP

    GWC -->|Update State| STATE
    GW -->|Update State| STATE
    HTTP -->|Update State| STATE
    GRPC -->|Update State| STATE
    IP -->|Update State| STATE

    GWC -->|ReferenceValidateRequest| RVS
    GW -->|ReferenceValidateRequest| RVS
    HTTP -->|ReferenceValidateRequest| RVS
    GRPC -->|ReferenceValidateRequest| RVS

    RVS -->|Read| STATE
    RVS -->|GatewayDeployRequest| GDS

    GDS -->|Route to Backend| ENVOY_XDS
    GDS -->|Route to Backend| ENVOY_CM
    GDS -->|Route to Backend| AGENT_GW

    ENVOY_XDS -->|xDS Protocol| ENVOY
    ENVOY_CM -->|ConfigMap Updates| K8S_API
    AGENT_GW -->|Create/Update Resources| K8S_WORKLOADS

    GWC -->|Patch Operations| PATCH_GWC
    GW -->|Patch Operations| PATCH_GW
    HTTP -->|Patch Operations| PATCH_HTTP
    GRPC -->|Patch Operations| PATCH_GRPC
    IP -->|Patch Operations| PATCH_IP

    PATCH_GWC -->|Status Updates| K8S_API
    PATCH_GW -->|Status Updates| K8S_API
    PATCH_HTTP -->|Status Updates| K8S_API
    PATCH_GRPC -->|Status Updates| K8S_API
    PATCH_IP -->|Status Updates| K8S_API

    style STATE fill:#f9f,stroke:#333,stroke-width:4px
    style RVS fill:#bbf,stroke:#333,stroke-width:2px
    style GDS fill:#bbf,stroke:#333,stroke-width:2px
```

## 2. HTTPRoute Processing Data Flow

```mermaid
sequenceDiagram
    participant K8S as Kubernetes API
    participant HTTP as HTTPRoute Controller
    participant STATE as State Store
    participant RVS as Reference Validator
    participant GDS as Gateway Deployer
    participant BACKEND as Backend (Envoy/AgentGateway)
    participant PATCHER as HTTPRoute Patcher

    K8S->>HTTP: HTTPRoute Created/Updated
    HTTP->>HTTP: Add Finalizer
    HTTP->>STATE: Attach Route to Gateways
    HTTP->>STATE: Save HTTPRoute
    HTTP->>RVS: ReferenceValidateRequest::AddRoute

    Note over RVS: Resolve Backend References
    RVS->>STATE: Get Services
    RVS->>STATE: Get ReferenceGrants
    RVS->>STATE: Check Cross-Namespace Access

    RVS->>RVS: Build Effective Gateway
    RVS->>GDS: GatewayDeployRequest

    Note over GDS: Route to Appropriate Backend
    GDS->>BACKEND: Deploy Configuration
    BACKEND->>BACKEND: Generate Backend Config
    BACKEND-->>GDS: BackendGatewayResponse

    GDS->>HTTP: Response via Channel
    HTTP->>PATCHER: PatchStatus Operation
    PATCHER->>K8S: Update HTTPRoute Status

    K8S-->>HTTP: Status Updated Event
    HTTP->>HTTP: Reconcile Complete
```

## 3. Reference Validation Flow

```mermaid
flowchart TD
    START([HTTPRoute/GRPCRoute Event])
    START --> EXTRACT[Extract Backend References]

    EXTRACT --> CHECK_SERVICES{Services Exist?}
    CHECK_SERVICES -->|No| MARK_UNRESOLVED[Mark Backend as Unresolved]
    CHECK_SERVICES -->|Yes| CHECK_NAMESPACE{Same Namespace?}

    CHECK_NAMESPACE -->|Yes| MARK_RESOLVED[Mark Backend as Resolved]
    CHECK_NAMESPACE -->|No| CHECK_GRANT{ReferenceGrant Exists?}

    CHECK_GRANT -->|No| MARK_NOT_ALLOWED[Mark Backend as NotAllowed]
    CHECK_GRANT -->|Yes| VALIDATE_GRANT{Grant Permits Access?}

    VALIDATE_GRANT -->|No| MARK_NOT_ALLOWED
    VALIDATE_GRANT -->|Yes| MARK_RESOLVED

    MARK_UNRESOLVED --> AGGREGATE[Aggregate All Backends]
    MARK_NOT_ALLOWED --> AGGREGATE
    MARK_RESOLVED --> AGGREGATE

    AGGREGATE --> CHECK_TLS{TLS Configured?}
    CHECK_TLS -->|No| BUILD_GATEWAY[Build Effective Gateway]
    CHECK_TLS -->|Yes| RESOLVE_SECRETS[Resolve TLS Secrets]

    RESOLVE_SECRETS --> BUILD_GATEWAY
    BUILD_GATEWAY --> DEPLOY[Send to Gateway Deployer]
    DEPLOY --> END([Complete])

    style MARK_UNRESOLVED fill:#faa,stroke:#333
    style MARK_NOT_ALLOWED fill:#faa,stroke:#333
    style MARK_RESOLVED fill:#afa,stroke:#333
```

## 4. Backend Selection and Deployment

```mermaid
flowchart LR
    subgraph "Gateway Deployer Service"
        GDS[Gateway Deploy Request]
        SELECTOR{Backend Type?}
    end

    subgraph "Envoy XDS Backend"
        XDS_CONVERT[Convert to xDS Config]
        XDS_LISTENER[Generate Listeners]
        XDS_CLUSTER[Generate Clusters]
        XDS_ROUTE[Generate Routes]
        XDS_GRPC[Serve via gRPC]
    end

    subgraph "Envoy ConfigMap Backend"
        CM_CONVERT[Convert to Envoy Config]
        CM_YAML[Generate YAML]
        CM_PATCH[Update ConfigMap]
    end

    subgraph "AgentGateway Backend"
        AG_CONVERT[Convert to K8s Resources]
        AG_DEPLOY[Create Deployment]
        AG_SVC[Create Service]
        AG_CONFIG[Create ConfigMap]
    end

    GDS --> SELECTOR
    SELECTOR -->|Envoy XDS| XDS_CONVERT
    SELECTOR -->|Envoy ConfigMap| CM_CONVERT
    SELECTOR -->|AgentGateway| AG_CONVERT

    XDS_CONVERT --> XDS_LISTENER
    XDS_LISTENER --> XDS_CLUSTER
    XDS_CLUSTER --> XDS_ROUTE
    XDS_ROUTE --> XDS_GRPC

    CM_CONVERT --> CM_YAML
    CM_YAML --> CM_PATCH

    AG_CONVERT --> AG_DEPLOY
    AG_CONVERT --> AG_SVC
    AG_CONVERT --> AG_CONFIG

    XDS_GRPC --> ENVOY[Envoy Proxy Fetches xDS]
    CM_PATCH --> K8S_CM[Kubernetes ConfigMap]
    AG_DEPLOY --> K8S_WORKLOAD[Kubernetes Workloads]
    AG_SVC --> K8S_WORKLOAD
    AG_CONFIG --> K8S_WORKLOAD

    style SELECTOR fill:#ff9,stroke:#333,stroke-width:3px
```

## 5. State Management Architecture

```mermaid
graph TD
    subgraph "State Store (Arc<Mutex<HashMap>>)"
        GWC_STATE[GatewayClass Storage]
        GW_STATE[Gateway Storage]
        HTTP_STATE[HTTPRoute Storage]
        GRPC_STATE[GRPCRoute Storage]
        IP_STATE[InferencePool Storage]
        GW_ROUTES[Gateway-Route Mappings]
        GW_TYPES[Gateway Implementation Types]
    end

    subgraph "Controllers (Write Access)"
        CTRL1[GatewayClass Controller]
        CTRL2[Gateway Controller]
        CTRL3[HTTPRoute Controller]
        CTRL4[GRPCRoute Controller]
        CTRL5[InferencePool Controller]
    end

    subgraph "Services (Read Access)"
        SRV1[Reference Validator]
        SRV2[Gateway Deployer]
        SRV3[Route Listener Matcher]
    end

    CTRL1 -->|save_gateway_class| GWC_STATE
    CTRL1 -->|save_gateway_type| GW_TYPES
    CTRL2 -->|save_gateway| GW_STATE
    CTRL3 -->|save_http_route| HTTP_STATE
    CTRL3 -->|attach_http_route_to_gateway| GW_ROUTES
    CTRL4 -->|save_grpc_route| GRPC_STATE
    CTRL5 -->|save_inference_pool| IP_STATE

    SRV1 -.->|get_gateway| GW_STATE
    SRV1 -.->|get_http_routes| HTTP_STATE
    SRV1 -.->|get_gateway_type| GW_TYPES
    SRV2 -.->|get_gateway| GW_STATE
    SRV2 -.->|get_http_routes_attached_to_gateway| GW_ROUTES
    SRV3 -.->|get_http_routes_attached_to_gateway| GW_ROUTES

    style GWC_STATE fill:#e6f3ff,stroke:#333
    style GW_STATE fill:#e6f3ff,stroke:#333
    style HTTP_STATE fill:#e6f3ff,stroke:#333
    style GRPC_STATE fill:#e6f3ff,stroke:#333
    style IP_STATE fill:#e6f3ff,stroke:#333
```

## 6. Channel-Based Communication Architecture

```mermaid
graph LR
    subgraph "Controllers"
        GWC_CTRL[GatewayClass<br/>Controller]
        GW_CTRL[Gateway<br/>Controller]
        HTTP_CTRL[HTTPRoute<br/>Controller]
        GRPC_CTRL[GRPCRoute<br/>Controller]
        IP_CTRL[InferencePool<br/>Controller]
    end

    subgraph "MPSC Channels"
        REF_CH([Reference<br/>Validate<br/>Channel])
        GWC_PATCH_CH([GatewayClass<br/>Patch Channel])
        GW_PATCH_CH([Gateway<br/>Patch Channel])
        HTTP_PATCH_CH([HTTPRoute<br/>Patch Channel])
        GRPC_PATCH_CH([GRPCRoute<br/>Patch Channel])
        IP_PATCH_CH([InferencePool<br/>Patch Channel])
        GW_DEPLOY_CH([Gateway<br/>Deploy<br/>Channel])
        BACKEND_CH([Backend<br/>Response<br/>Channel])
    end

    subgraph "Services"
        REF_SRV[Reference<br/>Validator<br/>Service]
        GW_DEPLOY_SRV[Gateway<br/>Deployer<br/>Service]
        GWC_PATCHER[GatewayClass<br/>Patcher]
        GW_PATCHER[Gateway<br/>Patcher]
        HTTP_PATCHER[HTTPRoute<br/>Patcher]
        GRPC_PATCHER[GRPCRoute<br/>Patcher]
        IP_PATCHER[InferencePool<br/>Patcher]
    end

    GWC_CTRL -->|ReferenceValidateRequest| REF_CH
    GW_CTRL -->|ReferenceValidateRequest| REF_CH
    HTTP_CTRL -->|ReferenceValidateRequest| REF_CH
    GRPC_CTRL -->|ReferenceValidateRequest| REF_CH

    REF_CH --> REF_SRV
    REF_SRV -->|GatewayDeployRequest| GW_DEPLOY_CH
    GW_DEPLOY_CH --> GW_DEPLOY_SRV
    GW_DEPLOY_SRV -->|BackendGatewayResponse| BACKEND_CH

    GWC_CTRL -->|PatchOperation| GWC_PATCH_CH
    GW_CTRL -->|PatchOperation| GW_PATCH_CH
    HTTP_CTRL -->|PatchOperation| HTTP_PATCH_CH
    GRPC_CTRL -->|PatchOperation| GRPC_PATCH_CH
    IP_CTRL -->|PatchOperation| IP_PATCH_CH

    GWC_PATCH_CH --> GWC_PATCHER
    GW_PATCH_CH --> GW_PATCHER
    HTTP_PATCH_CH --> HTTP_PATCHER
    GRPC_PATCH_CH --> GRPC_PATCHER
    IP_PATCH_CH --> IP_PATCHER

    style REF_CH fill:#ffd,stroke:#333,stroke-width:2px
    style GW_DEPLOY_CH fill:#ffd,stroke:#333,stroke-width:2px
```

## 7. Gateway API Resource Relationships

```mermaid
erDiagram
    GATEWAYCLASS ||--o{ GATEWAY : "controls"
    GATEWAY ||--o{ HTTPROUTE : "parent of"
    GATEWAY ||--o{ GRPCROUTE : "parent of"
    HTTPROUTE }o--o{ SERVICE : "references"
    GRPCROUTE }o--o{ SERVICE : "references"
    HTTPROUTE }o--o{ INFERENCEPOOL : "references"
    GATEWAY ||--o{ LISTENER : "has"
    LISTENER ||--o{ CERTIFICATE : "uses"
    REFERENCEGRANT ||--o{ SERVICE : "permits access"

    GATEWAYCLASS {
        string controllerName
        string backend_type
    }

    GATEWAY {
        string gatewayClassName
        array listeners
        string implementation_type
    }

    LISTENER {
        string protocol
        int port
        string hostname
        object tls
    }

    HTTPROUTE {
        array parentRefs
        array hostnames
        array rules
    }

    GRPCROUTE {
        array parentRefs
        array hostnames
        array rules
    }

    SERVICE {
        string name
        string namespace
        int port
    }

    INFERENCEPOOL {
        string name
        array endpoints
    }

    REFERENCEGRANT {
        array from
        array to
    }
```

## 8. Startup and Initialization Sequence

```mermaid
sequenceDiagram
    participant MAIN as Main
    participant STATE as State Store
    participant K8S as Kubernetes Client
    participant PATCHERS as Patcher Services
    participant VALIDATORS as Reference Validators
    participant DEPLOYERS as Gateway Deployers
    participant CONTROLLERS as Controllers

    MAIN->>STATE: Create State::new()
    MAIN->>K8S: Initialize Kubernetes Client

    Note over MAIN: Create MPSC Channels (16 total)
    MAIN->>MAIN: Create reference_validate_channel
    MAIN->>MAIN: Create gateway_deploy_channel
    MAIN->>MAIN: Create patcher_channels (5)

    Note over MAIN: Start Service Tasks
    MAIN->>PATCHERS: Spawn GatewayClass Patcher
    MAIN->>PATCHERS: Spawn Gateway Patcher
    MAIN->>PATCHERS: Spawn HTTPRoute Patcher
    MAIN->>PATCHERS: Spawn GRPCRoute Patcher
    MAIN->>PATCHERS: Spawn InferencePool Patcher

    MAIN->>VALIDATORS: Spawn Reference Validator Service
    VALIDATORS->>STATE: Initialize with State handle

    MAIN->>DEPLOYERS: Spawn Gateway Deployer Service
    DEPLOYERS->>DEPLOYERS: Initialize Backends (Envoy/AgentGateway)

    Note over MAIN: Start Controllers
    MAIN->>CONTROLLERS: Spawn GatewayClass Controller
    MAIN->>CONTROLLERS: Spawn Gateway Controller
    MAIN->>CONTROLLERS: Spawn HTTPRoute Controller
    MAIN->>CONTROLLERS: Spawn GRPCRoute Controller
    MAIN->>CONTROLLERS: Spawn InferencePool Controller

    CONTROLLERS->>K8S: Start Watch Streams

    Note over MAIN: All Systems Operational
    MAIN->>MAIN: await tokio::signal::ctrl_c()
```

## Key Architectural Principles

1. **Separation of Concerns**: Controllers handle Kubernetes reconciliation, Services handle business logic, Backends handle implementation details
2. **Channel-Based Decoupling**: MPSC channels provide async, non-blocking communication between components
3. **State Centralization**: Single source of truth for runtime state with mutex-protected access
4. **Plugin Architecture**: Multiple backend implementations (Envoy XDS, Envoy ConfigMap, AgentGateway) via trait abstraction
5. **Idempotent Operations**: All operations are designed to be safely retried
6. **Graceful Degradation**: Errors result in status updates and requeue, not crashes
