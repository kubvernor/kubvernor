// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use eater_domainmatcher::DomainPattern;
use log::{debug, warn};

use crate::common::DEFAULT_ROUTE_HOSTNAME;

const TARGET: &str = super::TARGET;

pub struct HostnameMatchFilter<'a> {
    listener_hostname: &'a str,
    route_hostnames: &'a [String],
}

impl<'a> HostnameMatchFilter<'a> {
    pub fn new(listener_hostname: &'a str, route_hostnames: &'a [String]) -> Self {
        Self { listener_hostname, route_hostnames }
    }

    pub fn filter(&self) -> bool {
        if !self.route_hostnames.is_empty() && self.listener_hostname.is_empty() {
            return true;
        }

        if self.route_hostnames.is_empty() {
            return true;
        }

        if let Some(hostname) = self.route_hostnames.first()
            && hostname == DEFAULT_ROUTE_HOSTNAME
        {
            return true;
        }

        let listener_hostname = self.listener_hostname.to_owned();

        let pattern = if let Some(stripped) = listener_hostname.strip_prefix("*.") {
            format! {"**+.{stripped}"}
        } else {
            listener_hostname.clone()
        };

        let mut wildcard_route_hostnames = vec![];
        if let Ok(pattern) = DomainPattern::<'_, '.'>::try_from(pattern.as_str()) {
            let maybe_filtered = self
                .route_hostnames
                .iter()
                .filter(|r: &&String| {
                    let res = pattern.matches(r);
                    debug!(target: TARGET,"Comparing hostnames {listener_hostname} {r} {res}");
                    if r.starts_with("*.") {
                        wildcard_route_hostnames.push(*r);
                    }
                    res
                })
                .nth(0)
                .is_some();
            if maybe_filtered {
                true
            } else if wildcard_route_hostnames.is_empty() {
                false
            } else {
                for wildcarded_route in wildcard_route_hostnames {
                    if let Ok(pattern) = DomainPattern::<'_, '.'>::try_from(wildcarded_route.as_str()) {
                        let res = pattern.matches(&listener_hostname);
                        if res {
                            debug!(target: TARGET,"Comparing wildcarded hostnames {listener_hostname} {wildcarded_route} {res}");
                            return true;
                        }
                    }
                }
                false
            }
        } else {
            warn!(target: TARGET,"Hostname is not a valid domain {}", &listener_hostname);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_testing() {
        let listener_hostname = "test.com";
        let route_hostnames = vec!["test.com".to_owned(), "no-test.com".to_owned()];
        assert!(HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());
        let route_hostnames = vec!["diff-test.com".to_owned(), "no-test.com".to_owned()];
        assert!(!HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());
        let listener_hostname = "*.test.com";
        let route_hostnames = vec!["blah.test.com".to_owned(), "no-test.com".to_owned()];
        assert!(HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());

        let listener_hostname = "*.test.com";
        let route_hostnames = vec!["test.com".to_owned(), "no-test.com".to_owned()];
        assert!(!HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());
        let listener_hostname = "*.test.com";
        let route_hostnames = vec!["*.test.com".to_owned(), "no-test.com".to_owned()];
        assert!(HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());

        let listener_hostname = "*.test.com";
        let route_hostnames = vec!["even.more.test.com".to_owned(), "no-test.com".to_owned()];
        assert!(HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());

        let listener_hostname = "more.test.com";
        let route_hostnames = vec!["*.test.com".to_owned()];
        assert!(HostnameMatchFilter::new(listener_hostname, &route_hostnames).filter());
    }
}
