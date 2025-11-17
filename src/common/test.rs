use std::collections::BTreeSet;

use gateway_api::httproutes::{HTTPRoute, HTTPRouteRule, RouteMatch};

use crate::common::ListenerCondition;

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
    let x: RouteMatch = serde_yaml::from_str(m).unwrap();
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
    let x: HTTPRouteRule = serde_yaml::from_str(m).unwrap();
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
