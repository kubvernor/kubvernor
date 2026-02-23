// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use gateway_api_inference_extension::inferencepools::InferencePool;
use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};

#[derive(TypedBuilder)]
pub struct InferencePoolPatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<InferencePool>>,
}

impl Patcher<InferencePool> for InferencePoolPatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<InferencePool>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<InferencePool> {
        Api::namespaced(self.client.clone(), namespace)
    }
}
