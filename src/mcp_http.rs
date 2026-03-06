// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! HTTP+SSE transport for the MCP server.
//!
//! Supports both the legacy SSE transport (2024-11-05 spec) and the newer
//! Streamable HTTP transport (2025-03-26 spec) for remote access.

use crate::error::GwsError;
use crate::mcp_server::ServerConfig;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

/// Per-session state for each connected MCP client.
struct Session {
    config: ServerConfig,
    tools_cache: Option<Vec<Value>>,
    user_token: Option<String>,
    sse_tx: Option<mpsc::Sender<SseEvent>>,
    created_at: Instant,
}

/// Internal SSE event wrapper.
struct SseEvent {
    event: String,
    data: String,
}

/// Shared application state across all sessions.
struct AppState {
    sessions: RwLock<HashMap<String, Session>>,
    default_config: ServerConfig,
}

/// Start the HTTP+SSE MCP server.
pub async fn start_http(config: ServerConfig, host: &str, port: u16) -> Result<(), GwsError> {
    let state = Arc::new(AppState {
        sessions: RwLock::new(HashMap::new()),
        default_config: config,
    });

    // Spawn background session cleanup task
    let cleanup_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let mut sessions = cleanup_state.sessions.write().await;
            let before = sessions.len();
            sessions.retain(|_id, session| {
                let expired = session.created_at.elapsed().as_secs() > 1800; // 30 minutes
                let sse_closed = session
                    .sse_tx
                    .as_ref()
                    .is_some_and(|tx| tx.is_closed());
                !expired && !sse_closed
            });
            let removed = before - sessions.len();
            if removed > 0 {
                eprintln!("[gws mcp-http] Cleaned up {removed} expired session(s)");
            }
        }
    });

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/mcp", post(mcp_post_handler))
        .route("/mcp", delete(mcp_delete_handler))
        .route("/health", get(health_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    if host != "127.0.0.1" && host != "localhost" {
        eprintln!(
            "[gws mcp-http] WARNING: Binding to {host}:{port} — the server will be accessible from the network."
        );
    }

    let addr = format!("{host}:{port}");
    eprintln!("[gws mcp-http] Listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| GwsError::Other(anyhow::anyhow!("Failed to bind to {}: {}", addr, e)))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| GwsError::Other(anyhow::anyhow!("Server error: {}", e)))?;

    Ok(())
}

/// Extract a bearer token from the Authorization header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Resolve session ID from either the `Mcp-Session-Id` header or `sessionId` query param.
fn resolve_session_id(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| query.get("sessionId").cloned())
}

// ---------- handlers ----------

/// GET /health — simple health check.
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// GET /sse — legacy SSE transport (MCP 2024-11-05 spec).
///
/// Creates a new session and returns an SSE stream. The first event sent is
/// `endpoint` with the POST URL for sending JSON-RPC messages.
async fn sse_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let token = extract_bearer_token(&headers);

    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<SseEvent>(32);

    let session = Session {
        config: state.default_config.clone(),
        tools_cache: None,
        user_token: token,
        sse_tx: Some(tx.clone()),
        created_at: Instant::now(),
    };

    state
        .sessions
        .write()
        .await
        .insert(session_id.clone(), session);

    // Send the initial endpoint event via a spawned task
    let endpoint_session_id = session_id.clone();
    tokio::spawn(async move {
        let endpoint_url = format!("/mcp?sessionId={}", endpoint_session_id);
        let _ = tx
            .send(SseEvent {
                event: "endpoint".to_string(),
                data: endpoint_url,
            })
            .await;
    });

    let stream = ReceiverStream::new(rx).map(|sse_event| {
        Ok(Event::default()
            .event(sse_event.event)
            .data(sse_event.data))
    });

    Ok(Sse::new(stream))
}

/// POST /mcp — JSON-RPC handler.
///
/// Supports both Streamable HTTP (Mcp-Session-Id header) and legacy SSE
/// (sessionId query param) session identification. For `initialize` requests
/// without a session, a new session is created automatically.
async fn mcp_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let token = extract_bearer_token(&headers);
    let session_id = resolve_session_id(&headers, &query);

    let method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let params = body
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let req_id = body.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = body.get("id").is_none();

    // For initialize, create a new session if none exists
    let effective_session_id = if method == "initialize" && session_id.is_none() {
        let new_id = uuid::Uuid::new_v4().to_string();
        let session = Session {
            config: state.default_config.clone(),
            tools_cache: None,
            user_token: token.clone(),
            sse_tx: None,
            created_at: Instant::now(),
        };
        state
            .sessions
            .write()
            .await
            .insert(new_id.clone(), session);
        Some(new_id)
    } else {
        session_id
    };

    let Some(sid) = effective_session_id else {
        let err_resp = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32600,
                "message": "Missing session. Send an initialize request or include Mcp-Session-Id header."
            }
        });
        return (StatusCode::BAD_REQUEST, HeaderMap::new(), Json(err_resp));
    };

    // Look up session and process the request
    let mut sessions = state.sessions.write().await;
    let Some(session) = sessions.get_mut(&sid) else {
        let err_resp = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32600,
                "message": format!("Unknown session: {}", sid)
            }
        });
        return (StatusCode::NOT_FOUND, HeaderMap::new(), Json(err_resp));
    };

    // Use request-level token if provided, otherwise fall back to session token
    let effective_token = token.as_deref().or(session.user_token.as_deref());

    let result = crate::mcp_server::handle_request(
        method,
        &params,
        &session.config,
        &mut session.tools_cache,
        effective_token,
    )
    .await;

    // If this is an SSE session, send the response through the SSE channel
    if let Some(tx) = &session.sse_tx {
        let response = match result {
            Ok(res) => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": res
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32603,
                    "message": e.to_string()
                }
            }),
        };
        let data =
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        let _ = tx
            .send(SseEvent {
                event: "message".to_string(),
                data,
            })
            .await;

        // For SSE transport, return 202 Accepted (response goes through SSE stream)
        let mut resp_headers = HeaderMap::new();
        if let Ok(val) = sid.parse() {
            resp_headers.insert("mcp-session-id", val);
        }
        return (StatusCode::ACCEPTED, resp_headers, Json(json!({})));
    }

    // For Streamable HTTP transport, return response directly
    let response = if is_notification {
        json!({})
    } else {
        match result {
            Ok(res) => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": res
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32603,
                    "message": e.to_string()
                }
            }),
        }
    };

    let mut resp_headers = HeaderMap::new();
    if let Ok(val) = sid.parse() {
        resp_headers.insert("mcp-session-id", val);
    }

    (StatusCode::OK, resp_headers, Json(response))
}

/// DELETE /mcp — terminate a session.
async fn mcp_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let session_id = resolve_session_id(&headers, &query);

    let Some(sid) = session_id else {
        return StatusCode::BAD_REQUEST;
    };

    let removed = state.sessions.write().await.remove(&sid).is_some();
    if removed {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bearer_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(
            extract_bearer_token(&headers),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_extract_bearer_token_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_extract_bearer_token_wrong_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_resolve_session_id_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "sess-123".parse().unwrap());
        let query = HashMap::new();
        assert_eq!(
            resolve_session_id(&headers, &query),
            Some("sess-123".to_string())
        );
    }

    #[test]
    fn test_resolve_session_id_from_query() {
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("sessionId".to_string(), "sess-456".to_string());
        assert_eq!(
            resolve_session_id(&headers, &query),
            Some("sess-456".to_string())
        );
    }

    #[test]
    fn test_resolve_session_id_header_takes_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "from-header".parse().unwrap());
        let mut query = HashMap::new();
        query.insert("sessionId".to_string(), "from-query".to_string());
        assert_eq!(
            resolve_session_id(&headers, &query),
            Some("from-header".to_string())
        );
    }

    #[test]
    fn test_resolve_session_id_none() {
        let headers = HeaderMap::new();
        let query = HashMap::new();
        assert_eq!(resolve_session_id(&headers, &query), None);
    }
}
