#!/bin/bash
go test -v -count=1 -timeout=3h ./conformance --debug -run TestConformance --report-output="../kubvernor-inference-conformance-output-0.1.1.yaml" --organization=kubvernor --project=kubvernor --url=https://github.com/kubvernor/kubvernor --version=0.1.1  --contact=nowakd@gmail.com --allow-crds-mismatch
cd ..
