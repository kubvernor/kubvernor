cd conformance
go test -v -count=1 -timeout=3h ./conformance --debug -run TestKubvernorGatewayAPIConformanceExperimental --report-output="../kubernor-conformance-output-1.2.1.yaml" --organization=kubvernor --project=kubvernor --url=https://github.com/kubvernor --version=latest  --contact=nowakd@gmail.com
cd ..
