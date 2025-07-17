#!/bin/bash

docker stop envoy-gateway-control-plane
docker rm envoy-gateway-control-plane
docker network rm kind
