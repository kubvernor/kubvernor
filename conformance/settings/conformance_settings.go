package settings

import (
	"k8s.io/apimachinery/pkg/util/sets"	
	"sigs.k8s.io/gateway-api/conformance/tests"
	"sigs.k8s.io/gateway-api/conformance/utils/suite"
	"sigs.k8s.io/gateway-api/pkg/features"
)


// SupportLevel represents the level of support for a feature.
// See https://gateway-api.sigs.k8s.io/concepts/conformance/#2-support-levels.
type SupportLevel string

const (
	// Core features are portable and expected to be supported by every implementation of Gateway-API.
	Core SupportLevel = "core"

	// Extended features are those that are portable but not universally supported across implementations.
	// Those implementations that support the feature will have the same behavior and semantics.
	// It is expected that some number of roadmap features will eventually migrate into the Core.
	Extended SupportLevel = "extended"
)



// SkipTests is a list of tests that are skipped in the conformance suite.
var SkipTests = []suite.ConformanceTest{
	tests.GatewayStaticAddresses,
	tests.GatewayInfrastructure,
}

func skipTestsShortNames(skipTests []suite.ConformanceTest) []string {
	shortNames := make([]string, len(skipTests))
	for i, test := range skipTests {
		shortNames[i] = test.ShortName
	}
	return shortNames
}

// KubvernorGatewaySuite is the conformance suite configuration for the Gateway API.
var KubvernorGatewaySuite = suite.ConformanceOptions{
	SupportedFeatures: supportedFeatures(),
	ExemptFeatures:    exemptFeatures(),
	SkipTests:         skipTestsShortNames(SkipTests),
}

func allFeatures() sets.Set[features.FeatureName] {
	allFeatures := sets.New[features.FeatureName]()
	for _, feature := range features.AllFeatures.UnsortedList() {
		allFeatures.Insert(feature.Name)
	}
	return allFeatures
}

func supportedFeatures() sets.Set[features.FeatureName] {
	supportedFeatures := sets.New[features.FeatureName]()
	for _, feature := range features.GatewayCoreFeatures.UnsortedList() {
		supportedFeatures.Insert(feature.Name)
	}

	for _, feature := range features.HTTPRouteCoreFeatures.UnsortedList() {
		supportedFeatures.Insert(feature.Name)
	}

	return supportedFeatures
}

func exemptFeatures() sets.Set[features.FeatureName] {
	exemptFeatures := sets.New[features.FeatureName]()
	for _, feature := range features.MeshCoreFeatures.UnsortedList() {
		exemptFeatures.Insert(feature.Name)
	}
	for _, feature := range features.MeshExtendedFeatures.UnsortedList() {
		exemptFeatures.Insert(feature.Name)
	}
	exemptFeatures.Insert(features.ReferenceGrantFeature.Name)

	return exemptFeatures
}

// ExtendedFeatures is a list of supported Gateway-API features that are considered Extended.
var ExtendedFeatures = sets.New[features.FeatureName]()

func init() {
	featureLists := sets.New[features.Feature]().
		Insert(features.GatewayExtendedFeatures.UnsortedList()...).
		Insert(features.HTTPRouteExtendedFeatures.UnsortedList()...).
		Insert(features.MeshExtendedFeatures.UnsortedList()...)

	for _, feature := range featureLists.UnsortedList() {
		ExtendedFeatures.Insert(feature.Name)
	}

	SupportedProfiles.Insert(suite.GatewayHTTPConformanceProfileName)
}

var SupportedProfiles = sets.New[suite.ConformanceProfileName]()

// GetTestSupportLevel returns the SupportLevel for a conformance test.
// The support level is determined by the highest support level of the features.
func GetTestSupportLevel(test suite.ConformanceTest) SupportLevel {
	supportLevel := Core

	if ExtendedFeatures.HasAny(test.Features...) {
		supportLevel = Extended
	}

	return supportLevel
}

// GetFeatureSupportLevel returns the SupportLevel for a feature.
func GetFeatureSupportLevel(feature features.FeatureName) SupportLevel {
	supportLevel := Core
	if ExtendedFeatures.Has(feature) {
		supportLevel = Extended
	}

	return supportLevel
}


func meshFeatures() sets.Set[features.FeatureName] {
	meshFeatures := sets.New[features.FeatureName]()
	for _, feature := range features.MeshCoreFeatures.UnsortedList() {
		meshFeatures.Insert(feature.Name)
	}
	for _, feature := range features.MeshExtendedFeatures.UnsortedList() {
		meshFeatures.Insert(feature.Name)
	}
	return meshFeatures
}
