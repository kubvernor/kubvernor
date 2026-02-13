use std::cmp;

use gateway_api::grpcroutes::GrpcRouteMatch;
use log::debug;

use crate::common::{Backend, FilterHeaders};

const TARGET: &str = super::super::super::TARGET;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GRPCEffectiveRoutingRule {
    pub route_matcher: GrpcRouteMatch,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,
}

impl PartialOrd for GRPCEffectiveRoutingRule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Self::compare_matching(&self.route_matcher, &other.route_matcher))
    }
}

impl GRPCEffectiveRoutingRule {
    fn header_matching(this: &GrpcRouteMatch, other: &GrpcRouteMatch) -> std::cmp::Ordering {
        let matcher = super::HeaderComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_headers()
    }

    fn method_matching(this: &GrpcRouteMatch, other: &GrpcRouteMatch) -> std::cmp::Ordering {
        match (this.method.as_ref(), other.method.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_method), Some(other_method)) => {
                let cmp_method = this_method.method.cmp(&other_method.method);
                let cmp_service = this_method.service.cmp(&other_method.service);

                match (cmp_method, cmp_service) {
                    (cmp::Ordering::Equal, _) => cmp_service,
                    _ => cmp_method,
                }
            },
        }
    }

    fn compare_matching(this: &GrpcRouteMatch, other: &GrpcRouteMatch) -> std::cmp::Ordering {
        let method_match = Self::method_matching(this, other);
        let header_match = Self::header_matching(this, other);

        let result = if header_match == std::cmp::Ordering::Equal { method_match } else { header_match };

        debug!(target: TARGET,"Comparing {this:#?} {other:#?} {result:?} {header_match:?} {method_match:?}");
        result
    }
}
