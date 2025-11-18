use gateway_api::{
    common::HeaderMatch,
    httproutes::{HttpRouteRulesMatchesPathType, PathMatch, RouteMatch},
};

use super::HTTPEffectiveRoutingRule;

#[test]
pub fn test_headers_sorting_rules() {
    let mut rules = vec![
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "version".to_owned(), value: "one".to_owned(), ..Default::default() }]),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "version".to_owned(), value: "two".to_owned(), ..Default::default() }]),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![
                    HeaderMatch { name: "version".to_owned(), value: "two".to_owned(), ..Default::default() },
                    HeaderMatch { name: "color".to_owned(), value: "orange".to_owned(), ..Default::default() },
                ]),
                ..Default::default()
            },

            name: "route3".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "color".to_owned(), value: "blue".to_owned(), ..Default::default() }]),
                ..Default::default()
            },
            name: "route4.1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "color".to_owned(), value: "green".to_owned(), ..Default::default() }]),
                ..Default::default()
            },

            name: "route4.2".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "color".to_owned(), value: "red".to_owned(), ..Default::default() }]),
                ..Default::default()
            },

            name: "route5.1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                headers: Some(vec![HeaderMatch { name: "color".to_owned(), value: "yellow".to_owned(), ..Default::default() }]),
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
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/".to_owned()) }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/one".to_owned()) }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/two".to_owned()) }),
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
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/".to_owned()) }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/one".to_owned()) }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/two".to_owned()) }),
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
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/".to_owned()) }),
                ..Default::default()
            },
            name: "route0".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/one".to_owned()) }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/two".to_owned()) }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/".to_owned()) }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/blah".to_owned()) }),
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
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/".to_owned()) }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/".to_owned()) }),
                headers: Some(vec![HeaderMatch { name: "color".to_owned(), value: "green".to_owned(), ..Default::default() }]),
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
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/match/exact/one".to_owned()) }),
                ..Default::default()
            },
            name: "route3".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/match/exact".to_owned()) }),
                ..Default::default()
            },
            name: "route2".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::Exact), value: Some("/match".to_owned()) }),
                ..Default::default()
            },
            name: "route1".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/match".to_owned()) }),
                ..Default::default()
            },
            name: "route4".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/match/prefix".to_owned()) }),
                ..Default::default()
            },
            name: "route5".to_owned(),
            ..Default::default()
        },
        HTTPEffectiveRoutingRule {
            route_matcher: RouteMatch {
                path: Some(PathMatch {
                    r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
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
