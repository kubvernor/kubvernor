// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

mod gateway_class_patcher;
mod gateway_patcher;
mod grpc_route_patcher;
mod http_route_patcher;
mod inference_pool_patcher;
mod patcher;

pub use gateway_class_patcher::GatewayClassPatcherService;
pub use gateway_patcher::GatewayPatcherService;
pub use grpc_route_patcher::GRPCRoutePatcherService;
pub use http_route_patcher::HttpRoutePatcherService;
pub use inference_pool_patcher::InferencePoolPatcherService;
pub use patcher::{DeleteContext, FinalizerContext, Operation, PatchContext, Patcher};
