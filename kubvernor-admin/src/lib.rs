// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use kubvernor_common::{Result, configuration::AdminInterfaceConfiguration};
use log::info;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct AppState {
    // Add any state fields needed for admin operations
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminResponse {
    pub status: String,
    pub message: String,
}

pub async fn admin(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let response = AdminResponse { status: "ok".to_owned(), message: "Admin endpoint is running".to_owned() };

    (StatusCode::OK, Json(response))
}

fn create_router(state: Arc<AppState>) -> Router {
    Router::new().route("/admin", get(admin)).with_state(state)
}

#[allow(clippy::too_many_lines)]
pub async fn start(configuration: Option<AdminInterfaceConfiguration>) -> Result<()> {
    let state = Arc::new(AppState {});
    let app = create_router(state);
    info!("Kubvernor Admin interface... ");
    if let Some(configuration) = configuration {
        let listener = TcpListener::bind(configuration.address.to_ip()?).await?;
        let local_addr = listener.local_addr()?;
        log::info!("Admin server listening on http://{local_addr}");

        axum::serve(listener, app).await?;
    } else {
        info!("Kubvernor Admin interface not configured");
    }

    Ok(())
}
