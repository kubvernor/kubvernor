use std::cmp;

use gateway_api::{common::RequestRedirect, httproutes::RouteMatch};
use tracing::debug;

use crate::common::{Backend, FilterHeaders};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HTTPEffectiveRoutingRule {
    pub route_matcher: RouteMatch,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,

    pub redirect_filter: Option<RequestRedirect>,
}

impl PartialOrd for HTTPEffectiveRoutingRule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Self::compare_matching(&self.route_matcher, &other.route_matcher))
    }
}

impl HTTPEffectiveRoutingRule {
    fn header_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let matcher = super::HeaderComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_headers()
    }

    fn query_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let matcher = super::QueryComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_queries()
    }

    fn method_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        match (this.method.as_ref(), other.method.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_method), Some(other_method)) => {
                let this_desc = this_method.clone() as isize;
                let other_desc = other_method.clone() as isize;
                this_desc.cmp(&other_desc)
            }
        }
    }
    fn path_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        match (this.path.as_ref(), other.path.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_path), Some(other_path)) => match (this_path.r#type.as_ref(), other_path.r#type.as_ref()) {
                (None, None) => this_path.value.cmp(&other_path.value),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(this_prefix_match_type), Some(other_prefix_match_type)) => {
                    let this_desc = this_prefix_match_type.clone() as isize;
                    let other_desc = other_prefix_match_type.clone() as isize;
                    let maybe_equal = this_desc.cmp(&other_desc);
                    if maybe_equal == cmp::Ordering::Equal {
                        match (&this_path.value, &other_path.value) {
                            (None, None) => std::cmp::Ordering::Equal,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (Some(this_path), Some(other_path)) => other_path.len().cmp(&this_path.len()),
                        }
                    } else {
                        maybe_equal
                    }
                }
            },
        }
    }

    fn compare_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let path_match = Self::path_matching(this, other);
        let method_match = Self::method_matching(this, other);
        let header_match = Self::header_matching(this, other);
        let query_match = Self::query_matching(this, other);
        let result = if query_match == std::cmp::Ordering::Equal {
            if header_match == std::cmp::Ordering::Equal {
                if path_match == std::cmp::Ordering::Equal {
                    method_match
                } else {
                    path_match
                }
            } else {
                header_match
            }
        } else {
            query_match
        };
        debug!("Comparing {this:#?} {other:#?} {result:?} {path_match:?} {header_match:?} {query_match:?} {method_match:?}");
        result
    }
}
