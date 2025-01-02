use std::collections::BTreeSet;

use gateway_api::apis::standard::httproutes::{HTTPRoute, HTTPRouteRules, HTTPRouteRulesMatches, HTTPRouteRulesMatchesHeaders, HTTPRouteRulesMatchesPath, HTTPRouteRulesMatchesPathType};

use crate::common::{EffectiveRoutingRule, ListenerCondition};

#[test]
pub fn test_enums() {
    let r1 = super::ResolvedRefs::Resolved(vec!["blah".to_owned()]);
    let r2 = super::ResolvedRefs::Resolved(vec!["blah2".to_owned()]);
    let d1 = r1.discriminant();
    let d2 = r2.discriminant();
    println!("{d1} {d2} {:?}", d1.cmp(&d2));
    assert_eq!(d1, d2);
    let e1 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::Resolved(vec!["blah".to_owned()]));
    let e2 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::Resolved(vec!["blah2".to_owned()]));
    let e3 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::ResolvedWithNotAllowedRoutes(vec![]));
    let e4 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::InvalidAllowedRoutes);
    let e5 = ListenerCondition::Accepted;
    assert_eq!(e1, e2);
    assert_eq!(e1, e3);
    assert_eq!(e1, e4);
    assert_ne!(e1, e5);
    let d1 = e1.discriminant();
    let d2 = e3.discriminant();

    println!("{d1:?} {d2:?} {:?}", d1.cmp(&d2));

    let mut set = BTreeSet::new();
    set.replace(e1);
    set.replace(e3);
    set.replace(e2);
    set.replace(e4);
    assert_eq!(set.len(), 1);
}

#[test]
pub fn test_rule_matcher() {
    let m = r"
path:
  type: PathPrefix
  value: /v2
headers:
- name: version
  value: two
";
    let x: HTTPRouteRulesMatches = serde_yaml::from_str(m).unwrap();
    println!("{x:#?}");
}

#[test]
pub fn test_route_rules() {
    let m = r"
matches:
  - path:
      type: PathPrefix
      value: /v2
  - headers:
    - name: version
      value: two
backendRefs:
  - name: infra-backend-v2
    port: 8080
";
    let x: HTTPRouteRules = serde_yaml::from_str(m).unwrap();
    println!("{x:#?}");
}

#[test]
pub fn test_http_route() {
    let m = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: matching-part1
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: same-namespace
  hostnames:
  - example.com
  - example.net
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /
      headers:
      - name: version
        value: one        
      
    - headers:
      - name: version
        value: one
    backendRefs:
    - name: infra-backend-v1
      port: 8080
  - matches:
    - path:
        type: Exact
        value: blah
    - headers:
      - name: version
        value: three
    backendRefs:
    - name: infra-backend-v2
      port: 8080
";
    let x: HTTPRoute = serde_yaml::from_str(m).unwrap();
    println!("{x:#?}");
}

#[test]
pub fn test_headers_sorting_rules() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "version".to_owned(),
                    value: "one".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "version".to_owned(),
                    value: "two".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![
                    HTTPRouteRulesMatchesHeaders {
                        name: "version".to_owned(),
                        value: "two".to_owned(),
                        ..Default::default()
                    },
                    HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "orange".to_owned(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },

            name: "route3".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "color".to_owned(),
                    value: "blue".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            name: "route4.1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "color".to_owned(),
                    value: "green".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },

            name: "route4.2".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "color".to_owned(),
                    value: "red".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },

            name: "route5.1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "color".to_owned(),
                    value: "yellow".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            name: "route5.2".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route3", "route1", "route2", "route4.1", "route4.2", "route5.1", "route5.2"];
    println!("{rules:#?}");
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}

#[test]
pub fn test_path_sorting_rules() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/".to_owned()),
                }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/one".to_owned()),
                }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/two".to_owned()),
                }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route1", "route2", "route3"];
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}

#[test]
pub fn test_path_prefix_sorting_rules() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/".to_owned()),
                }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/one".to_owned()),
                }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/two".to_owned()),
                }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route1", "route2", "route3"];
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}

#[test]
pub fn test_path_mixed_sorting_rules() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/".to_owned()),
                }),
                ..Default::default()
            },
            name: "route0".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/one".to_owned()),
                }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/two".to_owned()),
                }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/".to_owned()),
                }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/blah".to_owned()),
                }),
                ..Default::default()
            },
            name: "route4".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route4", "route3", "route1", "route2", "route0"];
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}

#[test]
pub fn test_paths_and_headers_sorting_rules() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/".to_owned()),
                }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/".to_owned()),
                }),
                headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                    name: "color".to_owned(),
                    value: "green".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route2", "route1"];
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}

#[test]
pub fn test_path_sorting_rules_extended() {
    let mut rules = vec![
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/match/exact/one".to_owned()),
                }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/match/exact".to_owned()),
                }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                    value: Some("/match".to_owned()),
                }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/match".to_owned()),
                }),
                ..Default::default()
            },
            name: "route4".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/match/prefix".to_owned()),
                }),
                ..Default::default()
            },
            name: "route5".to_owned(),
            ..Default::default()
        },
        EffectiveRoutingRule {
            route_matcher: HTTPRouteRulesMatches {
                path: Some(HTTPRouteRulesMatchesPath {
                    r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                    value: Some("/match/prefix/one".to_owned()),
                }),
                ..Default::default()
            },
            name: "route6".to_owned(),
            ..Default::default()
        },
    ];
    let expected_names = vec!["route3", "route2", "route1", "route6", "route5", "route4"];
    rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
    let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
    assert_eq!(expected_names, actual_names);
}
