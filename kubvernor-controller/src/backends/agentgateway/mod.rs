// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

mod agentgateway_deployer;
mod converters;
pub(crate) mod resource_generator;
mod server;
use agentgateway_api_rs::agentgateway::dev::resource::Listener;
#[cfg(feature = "agentgateway")]
pub use agentgateway_deployer::AgentgatewayDeployerChannelHandlerService;

#[derive(Clone, PartialEq, Default)]
pub struct SecureListenerWrapper(Listener);

impl std::fmt::Debug for SecureListenerWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Listener")
            .field(&self.0.key)
            .field(&self.0.bind_key)
            .field(&self.0.name)
            .field(&self.0.hostname)
            .field(&self.0.protocol)
            .finish()
    }
}
impl From<SecureListenerWrapper> for Listener {
    fn from(value: SecureListenerWrapper) -> Self {
        value.0
    }
}

const TARGET: &str = "kubvernor::backend::agentgateway";
