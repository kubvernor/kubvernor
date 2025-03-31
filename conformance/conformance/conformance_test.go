// Copied from EnvoyProxy
//https://raw.githubusercontent.com/envoyproxy/gateway/refs/heads/main/test/conformance/conformance_test.go


// Copyright Envoy Gateway Authors
// SPDX-License-Identifier: Apache-2.0
// The full text of the Apache license is available in the LICENSE file at
// the root of the repo.

package conformance

import (
	"flag"
	"os"
	"testing"
	"time"
	"kubvernor_conformance_test/settings"
	"sigs.k8s.io/gateway-api/conformance"
	"sigs.k8s.io/gateway-api/conformance/tests"
	"sigs.k8s.io/gateway-api/conformance/utils/config"
	"sigs.k8s.io/gateway-api/conformance/utils/suite"
	"sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	"github.com/stretchr/testify/require"
)






func TestKubvernorGatewayAPIConformance(t *testing.T) {
	flag.Parse()
	log.SetLogger(zap.New(zap.WriteTo(os.Stderr), zap.UseDevMode(true)))


	opts := conformance.DefaultOptions(t)
	opts.TimeoutConfig = config.TimeoutConfig{
		TestIsolation:                      10 * time.Second,
		// CreateTimeout:                      120 * time.Second,
		// DeleteTimeout:                      10 * time.Second,
		// GetTimeout:                         10 * time.Second,
		// GatewayMustHaveAddress:             180 * time.Second,
		// GatewayMustHaveCondition:           180 * time.Second,
		// GatewayStatusMustHaveListeners:     180 * time.Second,
		// GatewayListenersMustHaveConditions: 180 * time.Second,
		// GWCMustBeAccepted:                  180 * time.Second,
		// HTTPRouteMustNotHaveParents:        180 * time.Second,
		// HTTPRouteMustHaveCondition:         180 * time.Second,
		// TLSRouteMustHaveCondition:          60 * time.Second,
		// RouteMustHaveParents:               180 * time.Second,
		// ManifestFetchTimeout:               10 * time.Second,
		// MaxTimeToConsistency:               180 * time.Second,
		// NamespacesMustBeReady:              300 * time.Second,
		// RequestTimeout:                     10 * time.Second,
		// LatestObservedGenerationSet:        180 * time.Second,
		// DefaultTestTimeout:                 180 * time.Second,
		// RequiredConsecutiveSuccesses:       3,
	}


	opts.SkipTests = settings.KubvernorGatewaySuite.SkipTests
	opts.SupportedFeatures = settings.KubvernorGatewaySuite.SupportedFeatures
	opts.ExemptFeatures = settings.KubvernorGatewaySuite.ExemptFeatures

	opts.GatewayClassName = "kubvernor-gateway"
	opts.CleanupBaseResources = false

	cSuite, err := suite.NewConformanceTestSuite(opts)
	if err != nil {
		t.Fatalf("Error creating conformance test suite: %v", err)
	}
	cSuite.Setup(t, tests.ConformanceTests)
	if err := cSuite.Run(t, tests.ConformanceTests); err != nil {
		t.Fatalf("Error running conformance tests: %v", err)
	}

	if err != nil {
		t.Fatalf("error generating conformance profile report: %v", err)
	}

	require.NoError(t, err)

}






