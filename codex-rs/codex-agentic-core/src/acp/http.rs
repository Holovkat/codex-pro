use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use super::Invocation;
use super::Message;
use super::RunExecution;
use super::RuntimeOptions;
use super::TransportResponse;
use super::execute_invocation;
use super::execution_to_response;
use crate::CommandContext;
use crate::CommandRegistry;

#[derive(Clone)]
struct AppState {
    registry: Arc<CommandRegistry>,
    base_ctx: CommandContext,
    opts: RuntimeOptions,
    runs: Arc<Mutex<HashMap<Uuid, RunExecution>>>,
}

pub async fn run(
    opts: RuntimeOptions,
    registry: Arc<CommandRegistry>,
    base_ctx: CommandContext,
) -> Result<()> {
    let listener = TcpListener::bind(&opts.listen)
        .await
        .with_context(|| format!("failed to bind ACP HTTP listener on {}", opts.listen))?;
    info!("ACP HTTP server listening on {}", opts.listen);

    let app_state = AppState {
        registry,
        base_ctx,
        opts: opts.clone(),
        runs: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/agents", get(list_agents))
        .route("/agents/:name", get(get_agent))
        .route("/runs", post(create_run))
        .route("/runs/:id", get(get_run))
        .route("/runs/:id/events", get(stream_run_events))
        .with_state(app_state);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("ACP HTTP server exited with error")?;

    Ok(())
}

async fn list_agents(State(state): State<AppState>) -> Json<AgentsResponse> {
    Json(AgentsResponse {
        agents: vec![build_agent_manifest(&state.opts)],
    })
}

async fn get_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<AgentManifest>, StatusCode> {
    if name.eq_ignore_ascii_case(&state.opts.agent_name) {
        return Ok(Json(build_agent_manifest(&state.opts)));
    }

    Err(StatusCode::NOT_FOUND)
}

async fn create_run(
    State(state): State<AppState>,
    Json(invocation): Json<Invocation>,
) -> Json<TransportResponse> {
    let execution = execute_invocation(
        Arc::clone(&state.registry),
        &state.base_ctx,
        &state.opts.agent_name,
        invocation,
    );

    let run_id = execution.run_id;
    {
        let mut runs = state.runs.lock().await;
        runs.insert(run_id, execution.clone());
    }

    Json(execution_to_response(execution, &state.opts.agent_name))
}

async fn get_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<TransportResponse>, StatusCode> {
    let run_id = Uuid::parse_str(&run_id).map_err(|_| StatusCode::NOT_FOUND)?;
    let runs = state.runs.lock().await;
    if let Some(execution) = runs.get(&run_id) {
        return Ok(Json(execution_to_response(
            execution.clone(),
            &state.opts.agent_name,
        )));
    }

    Err(StatusCode::NOT_FOUND)
}

async fn stream_run_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, StatusCode> {
    let run_id = Uuid::parse_str(&run_id).map_err(|_| StatusCode::NOT_FOUND)?;
    let runs = state.runs.lock().await;
    let Some(execution) = runs.get(&run_id) else {
        return Err(StatusCode::NOT_FOUND);
    };

    let payload = EventsResponse {
        run_id: execution.run_id,
        events: execution
            .output
            .iter()
            .map(|message| EventPayload {
                kind: "output".to_string(),
                message: message.clone(),
            })
            .collect(),
    };

    let json = serde_json::to_string(&payload).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json,
    )
        .into_response())
}

#[derive(Serialize)]
struct AgentsResponse {
    agents: Vec<AgentManifest>,
}

#[derive(Serialize, Clone)]
struct AgentManifest {
    name: String,
    description: String,
    transports: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_url: Option<String>,
}

fn build_agent_manifest(opts: &RuntimeOptions) -> AgentManifest {
    let base_description = "Codex Agentic command surface exposed over ACP";
    let description = if let Some(summary) = opts.initial_status.as_deref() {
        format!("{base_description}\n\n{summary}")
    } else {
        base_description.to_string()
    };

    AgentManifest {
        name: opts.agent_name.clone(),
        description,
        transports: if opts.enable_http {
            vec!["stdio".to_string(), format!("http:{}", opts.listen)]
        } else {
            vec!["stdio".to_string()]
        },
        public_url: opts.public_url.clone(),
    }
}

#[derive(Serialize)]
struct EventsResponse {
    #[serde(serialize_with = "super::uuid_to_string")]
    run_id: Uuid,
    events: Vec<EventPayload>,
}

#[derive(Serialize)]
struct EventPayload {
    #[serde(rename = "type")]
    kind: String,
    message: Message,
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        warn!("ACP HTTP server shutdown signal listener failed");
    }
}
