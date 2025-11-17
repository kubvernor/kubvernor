mod grpc_route;
mod http_route;

use gateway_api::common::HeaderMatch;
pub use grpc_route::GRPCEffectiveRoutingRule;
pub use http_route::HTTPEffectiveRoutingRule;
use typed_builder::TypedBuilder;

#[cfg(test)]
mod test;

#[derive(TypedBuilder)]
struct HeaderComparator<'a> {
    this: Option<&'a Vec<HeaderMatch>>,
    other: Option<&'a Vec<HeaderMatch>>,
}

struct Comparator<'a, T> {
    this: Option<&'a Vec<T>>,
    other: Option<&'a Vec<T>>,
}

impl<T> Comparator<'_, T> {
    pub fn compare(self) -> std::cmp::Ordering {
        match (self.this, self.other) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_headers), Some(other_headers)) => other_headers.len().cmp(&this_headers.len()),
        }
    }
}

impl HeaderComparator<'_> {
    pub fn compare_headers(self) -> std::cmp::Ordering {
        let comp = Comparator { this: self.this, other: self.other };
        comp.compare()
    }
}

#[derive(TypedBuilder)]
struct QueryComparator<'a> {
    this: Option<&'a Vec<HeaderMatch>>,
    other: Option<&'a Vec<HeaderMatch>>,
}
impl QueryComparator<'_> {
    pub fn compare_queries(self) -> std::cmp::Ordering {
        let comp = Comparator { this: self.this, other: self.other };
        comp.compare()
    }
}
