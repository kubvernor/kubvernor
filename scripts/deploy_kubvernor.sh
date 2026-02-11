#!/bin/bash

dir=$(pwd)
location=$(dirname $0)
cd $location
./create_cluster.sh
sleep 2
kubectl create namespace gateway-conformance-infra
kubectl create namespace inference-conformance-infra
./load_images.sh
sleep 2
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.0/standard-install.yaml
kubectl apply -f ../kubernetes/kubvernor-crds.yaml
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/download/v1.1.0/manifests.yaml
kubectl apply -f ../kubernetes/kubvernor-config.yaml
sleep 2
kubectl apply -f ../kubernetes/kubvernor-deployment.yaml

cd $dir
