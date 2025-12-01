use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use axum::{routing::{get, post, put, delete}, Router, extract::{State, Query, FromRequestParts}, http::{request::Parts, header, StatusCode}, Json};
use tower_http::cors::{CorsLayer, Any};
use tracing_subscriber::{EnvFilter, fmt};
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use rcrt_core::{db::Db, models::{BreadcrumbCreate, BreadcrumbContextView, BreadcrumbFull, AclGrantAgent}};
use anyhow::Result;
use sqlx::migrate::Migrator;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde_json::json;
use chrono;

mod hygiene;
mod transforms;
mod embedding_policy;
#[cfg(feature = "nats")]
use nats;
use reqwest::Client as HttpClient;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use axum::response::{Html, IntoResponse};
use prometheus::{Encoder, TextEncoder, IntCounterVec, HistogramVec, register_int_counter_vec, register_histogram_vec};
use std::sync::OnceLock as StdOnceLock;
#[cfg(feature = "embed-onnx")]
use std::sync::OnceLock;
#[cfg(feature = "embed-onnx")]
use ort::{session::Session, value::Value, inputs};
#[cfg(feature = "embed-onnx")]
use tokenizers::Tokenizer;
// no ndarray tensors needed in embed path

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");


#[derive(Clone)]
struct AppState {
    db: Db,
    jwt_decoding_key: Option<DecodingKey>,
    jwt_encoding_key: Option<EncodingKey>,
    jwt_validation: Validation,
    #[cfg(feature = "nats")]
    nats_conn: Option<nats::Connection>,
    hygiene_stats: Arc<Mutex<hygiene::HygieneStats>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let filter = EnvFilter::from_default_env();
    fmt().with_env_filter(filter).init();

    let db_url = std::env::var("DB_URL").expect("DB_URL not set");
    let owner_id = std::env::var("OWNER_ID").ok()
        .and_then(|s| Uuid::parse_str(&s).ok())
        .unwrap_or_else(|| Uuid::new_v4());

    let db = Db::connect(&db_url, owner_id, None).await?;
    // Run migrations on startup
    MIGRATOR.run(&db.pool).await?;
    // Ensure default tenant exists (prevents FK violations on first boot)
    db.ensure_tenant(owner_id, "Default Tenant").await?;
    // JWT config (optional for now)
    let jwt_pub_pem = std::env::var("JWT_PUBLIC_KEY_PEM").ok();
    let jwt_priv_pem = std::env::var("JWT_PRIVATE_KEY_PEM").ok();
    let (jwt_decoding_key, jwt_encoding_key, jwt_validation) = if let Some(pub_pem) = jwt_pub_pem {
        let dec_key = DecodingKey::from_rsa_pem(pub_pem.as_bytes()).expect("invalid RSA pub key");
        let enc_key = jwt_priv_pem.as_ref().map(|priv_pem| {
            EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("invalid RSA private key")
        });
        (Some(dec_key), enc_key, Validation::new(Algorithm::RS256))
    } else { (None, None, Validation::new(Algorithm::RS256)) };

    // NATS (required when feature is enabled): fail fast if not reachable
    #[cfg(feature = "nats")]
    let nats_conn = {
        let nats_url = std::env::var("NATS_URL").expect("NATS_URL not set");
        let conn = nats::connect(&nats_url).expect("failed to connect to NATS");
        tracing::info!("✅ Connected to NATS at {}", nats_url);
        Some(conn)
    };

    // Create shared hygiene stats
    let hygiene_stats = Arc::new(Mutex::new(hygiene::HygieneStats::default()));
    
    #[cfg(feature = "nats")]
    let state = AppState { 
        db, 
        jwt_decoding_key, 
        jwt_encoding_key, 
        jwt_validation, 
        nats_conn,
        hygiene_stats: hygiene_stats.clone(),
    };
    #[cfg(not(feature = "nats"))]
    let state = AppState { 
        db, 
        jwt_decoding_key, 
        jwt_encoding_key, 
        jwt_validation,
        hygiene_stats: hygiene_stats.clone(),
    };

    // Start hygiene runner for automatic cleanup
    tracing::info!("Initializing hygiene system...");
    let hygiene_config = hygiene::load_hygiene_config();
    tracing::info!("Hygiene config loaded: enabled={}, interval={}s", hygiene_config.enabled, hygiene_config.run_interval_seconds);
    
    let hygiene_runner = hygiene::HygieneRunner::new(state.clone(), Some(hygiene_config));
    tracing::info!("Hygiene runner created, starting background task...");
    
    let hygiene_handle = hygiene_runner.start();
    tracing::info!("Hygiene runner started for automatic cleanup");
    
    // Don't drop the handle - store it to keep the task alive
    let _hygiene_task = hygiene_handle;

    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/", get(docs_page))
        .route("/docs", get(docs_page))
        .route("/swagger", get(swagger_page))
        .route("/openapi.json", get(openapi_spec))
        .route("/auth/token", post(generate_jwt_token))
        .route("/admin/purge", post(admin_purge))
        .route("/agents/run", post(run_agents))
        .route("/breadcrumbs", post(create_breadcrumb).get(list_breadcrumbs))
        .route("/breadcrumbs/:id", get(get_breadcrumb_context).patch(update_breadcrumb).delete(delete_breadcrumb))
        .route("/breadcrumbs/:id/full", get(get_breadcrumb_full))
        .route("/breadcrumbs/:id/history", get(get_breadcrumb_history))
        .route("/breadcrumbs/:id/tags/add", post(add_tags))
        .route("/breadcrumbs/:id/tags/remove", post(remove_tags))
        .route("/breadcrumbs/:id/context/merge", post(merge_context))
        .route("/breadcrumbs/:id/approve", post(approve_breadcrumb))
        .route("/breadcrumbs/:id/reject", post(reject_breadcrumb))
        .route("/breadcrumbs/search", get(vector_search))
        .route("/events/stream", get(sse_stream))
        .route("/acl", get(list_acls))
        .route("/acl/grant", post(grant_acl))
        .route("/acl/revoke", post(revoke_acl))
        .route("/agents", get(list_agents))
        .route("/agents/:id/webhooks", post(register_webhook).get(list_webhooks))
        .route("/agents/:id/webhooks/:wid", axum::routing::delete(deactivate_webhook))
        .route("/agents/:id", post(register_agent).get(get_agent).delete(delete_agent))
        .route("/agents/:id/secret", post(set_agent_secret))
        .route("/tenants", get(list_tenants))
        .route("/tenants/:id", post(ensure_tenant).get(get_tenant).put(update_tenant).delete(delete_tenant))
        .route("/secrets", post(create_secret).get(list_secrets))
        .route("/secrets/:id", put(update_secret).delete(delete_secret))
        .route("/secrets/:id/decrypt", post(decrypt_secret))
        .route("/dlq", get(list_dlq))
        .route("/dlq/:id", delete(delete_dlq))
        .route("/dlq/:id/retry", post(retry_dlq))
        .route("/hygiene/stats", get(get_hygiene_stats))
        .route("/hygiene/run", post(trigger_hygiene_run))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
        )
        .layer(axum::middleware::from_fn(http_metrics_middleware));

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    tracing::info!("listening on {}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
#[derive(Deserialize)]
struct SearchQuery { qvec: Option<String>, q: Option<String>, nn: Option<i64>, tag: Option<String>, schema_name: Option<String>, include_context: Option<bool> }

#[derive(Serialize)]
#[serde(untagged)]
enum SearchResult {
    List(Vec<ListItem>),
    Context(Vec<BreadcrumbContextView>),
}

async fn vector_search(State(state): State<AppState>, auth: AuthContext, Query(q): Query<SearchQuery>) -> Result<Json<SearchResult>, (StatusCode, String)> {
    // if qvec not provided, attempt to embed ?q=title/context
    let qvec: Vec<f32> = if let Some(qv) = q.qvec {
        qv.split(',').filter_map(|s| s.parse::<f32>().ok()).collect()
    } else if let Some(text) = q.q {
        match embed_text(text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("vector_search embedding failed: {}", e);
                return Ok(Json(SearchResult::List(vec![])));
            }
        }
    } else { return Ok(Json(SearchResult::List(vec![]))); };
    let limit = q.nn.unwrap_or(5).max(1) as i64;
    let include_context = q.include_context.unwrap_or(false);

    if include_context {
        // Return full context view
        let mut sql = String::from("select id, title, context, tags, schema_name, version, updated_at from breadcrumbs where owner_id = $1");
        let mut bind_idx = 3;
        if q.tag.is_some() { 
            sql.push_str(&format!(" and ${} = any(tags)", bind_idx)); 
            bind_idx += 1;
        }
        if q.schema_name.is_some() { 
            sql.push_str(&format!(" and schema_name = ${}", bind_idx)); 
        }
        sql.push_str(" order by embedding <#> $2::vector limit ");
        sql.push_str(&limit.to_string());

        let rows = match (&q.tag, &q.schema_name) {
            (Some(tag), Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(tag)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (Some(tag), None) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(tag)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, None) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            }
        };

        let items = rows.into_iter().map(|(id,title,context,tags,schema_name,version,updated_at)| {
            BreadcrumbContextView {
                id, title, description: None, semantic_version: None, context, tags, schema_name, llm_hints: None, version, updated_at
            }
        }).collect();
        Ok(Json(SearchResult::Context(items)))
    } else {
        // Return minimal list view
        let mut sql = String::from("select id, title, tags, version, updated_at from breadcrumbs where owner_id = $1");
        let mut bind_idx = 3;
        if q.tag.is_some() { 
            sql.push_str(&format!(" and ${} = any(tags)", bind_idx)); 
            bind_idx += 1;
        }
        if q.schema_name.is_some() { 
            sql.push_str(&format!(" and schema_name = ${}", bind_idx)); 
        }
        sql.push_str(" order by embedding <#> $2::vector limit ");
        sql.push_str(&limit.to_string());

        let rows = match (&q.tag, &q.schema_name) {
            (Some(tag), Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(tag)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (Some(tag), None) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(tag)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, None) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(auth.owner_id)
                    .bind(&qvec)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            }
        };

        let items = rows.into_iter().map(|(id,title,tags,version,updated_at)| ListItem{ id, title, tags, schema_name: None, version, updated_at }).collect();
        Ok(Json(SearchResult::List(items)))
    }
}

fn extract_text_for_embedding(bc: &rcrt_core::models::Breadcrumb) -> String {
    let mut text = bc.title.clone();
    
    // Add description if present
    if let Some(desc) = &bc.description {
        text.push_str(" ");
        text.push_str(desc);
    }
    
    // Try to apply llm_hints transform for human-readable text
    if let Some(hints_value) = &bc.llm_hints {
        if let Ok(hints) = serde_json::from_value::<transforms::LlmHints>(hints_value.clone()) {
            if let Ok(transformed) = transforms::TransformEngine::new().apply_llm_hints(&bc.context, &hints) {
                text.push_str(" ");
                text.push_str(&transformed.to_string());
                return text;
            }
        }
    }
    
    // Fallback: title + description only (avoid embedding raw JSON)
    text
}

fn extract_text_for_embedding_struct(req: &CreateReq) -> String {
    let mut text = req.title.clone();
    
    // Add description if present
    if let Some(desc) = &req.description {
        text.push_str(" ");
        text.push_str(desc);
    }
    
    // Try to apply llm_hints transform for human-readable text
    if let Some(hints_value) = &req.llm_hints {
        if let Ok(hints) = serde_json::from_value::<transforms::LlmHints>(hints_value.clone()) {
            if let Ok(transformed) = transforms::TransformEngine::new().apply_llm_hints(&req.context, &hints) {
                text.push_str(" ");
                text.push_str(&transformed.to_string());
                return text;
            }
        }
    }
    
    // Fallback: title + description only (avoid embedding raw JSON)
    text
}

#[cfg(feature = "embed-onnx")]
fn embed_text(text: String) -> Result<Vec<f32>, String> {
    static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();
    static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
    let tok = TOKENIZER.get_or_init(|| {
        let path = std::env::var("EMBED_TOKENIZER").unwrap_or_else(|_| "models/tokenizer.json".into());
        Tokenizer::from_file(path).expect("load tokenizer")
    });
    let session = SESSION.get_or_init(|| {
        let model_path = std::env::var("EMBED_MODEL").unwrap_or_else(|_| "models/model.onnx".into());
        Mutex::new(Session::builder().unwrap().commit_from_file(model_path).unwrap())
    });
    let encoding = tok.encode(text, true).map_err(|e| e.to_string())?;
    let ids = encoding.get_ids();
    let ids_vec: Vec<i64> = ids.iter().map(|&x| x as i64).collect();
    let shape: Vec<usize> = vec![1, ids.len()];
    let mask_vec: Vec<i64> = vec![1i64; ids.len()];
    let seg_vec: Vec<i64> = vec![0i64; ids.len()];

    // Try with common BERT-style inputs first; fall back to input_ids only if model rejects extra inputs
    let try_run = |with_all: bool| -> Result<Vec<f32>, String> {
        let mut guard = session.lock().unwrap();
        let outputs = if with_all {
            let inp = inputs!{
                "input_ids" => Value::from_array((shape.clone(), ids_vec.clone())).map_err(|e| e.to_string())?,
                "attention_mask" => Value::from_array((shape.clone(), mask_vec.clone())).map_err(|e| e.to_string())?,
                "token_type_ids" => Value::from_array((shape.clone(), seg_vec.clone())).map_err(|e| e.to_string())?
            };
            guard.run(inp).map_err(|e| e.to_string())?
        } else {
            let inp = inputs!{
                "input_ids" => Value::from_array((shape.clone(), ids_vec.clone())).map_err(|e| e.to_string())?
            };
            guard.run(inp).map_err(|e| e.to_string())?
        };
        let (_shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|_| "embedding output not a float tensor".to_string())?;
        let hidden: usize = std::env::var("EMBED_DIM").ok().and_then(|s| s.parse().ok()).unwrap_or(384usize);
        if data.len() == hidden {
            Ok(data.to_vec())
        } else {
            let mut acc = vec![0f32; hidden];
            let mut count: usize = 0;
            for chunk in data.chunks_exact(hidden) {
                for i in 0..hidden { acc[i] += chunk[i]; }
                count += 1;
            }
            if count > 0 { for i in 0..hidden { acc[i] /= count as f32; } }
            Ok(acc)
        }
    };
    let vec = match try_run(true) {
        Ok(v) => v,
        Err(e) => {
            // Retry with minimal inputs for models that don't expect mask/segment
            tracing::warn!("embed_text run with all inputs failed, retrying with input_ids only: {}", e);
            try_run(false)?
        }
    };
    // L2 normalize
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 { Ok(vec.into_iter().map(|x| x / norm).collect()) } else { Ok(vec) }
}
#[cfg(not(feature = "embed-onnx"))]
fn embed_text(_text: String) -> Result<Vec<f32>, String> { Err("embedding disabled".into()) }

async fn health() -> &'static str { "ok" }

#[derive(Deserialize)]
struct TokenRequest {
    owner_id: String,
    agent_id: String, 
    roles: Option<Vec<String>>,
    ttl_sec: Option<i64>,
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
    owner_id: String,
    agent_id: String,
    roles: Vec<String>,
    exp: i64,
}

async fn generate_jwt_token(State(state): State<AppState>, Json(req): Json<TokenRequest>) -> Result<Json<TokenResponse>, (StatusCode, String)> {
    let Some(encoding_key) = &state.jwt_encoding_key else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "JWT signing not configured (missing JWT_PRIVATE_KEY_PEM)".into()));
    };
    
    // Require explicit values in request - no environment fallbacks
    let owner_id = req.owner_id;
    let agent_id = req.agent_id;
    let roles = req.roles.unwrap_or_else(|| vec!["curator".into(), "emitter".into(), "subscriber".into()]);
    let ttl_sec = req.ttl_sec.unwrap_or(3600); // 1 hour default
    
    // Validate UUIDs
    let _owner_uuid = Uuid::parse_str(&owner_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid owner_id format".into()))?;
    let agent_uuid = Uuid::parse_str(&agent_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid agent_id format".into()))?;
    
    let now = chrono::Utc::now().timestamp();
    let exp = now + ttl_sec;
    
    let mut claims = json!({
        "sub": agent_id,
        "owner_id": owner_id,
        "roles": roles,
        "iat": now,
        "exp": exp
    });
    
    // Add optional issuer/audience if configured
    if let Ok(iss) = std::env::var("JWT_ISSUER") {
        claims["iss"] = json!(iss);
    }
    if let Ok(aud) = std::env::var("JWT_AUDIENCE") {
        claims["aud"] = json!(aud);
    }
    
    let header = Header::new(Algorithm::RS256);
    let token = encode(&header, &claims, encoding_key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("JWT encoding failed: {}", e)))?;
    
    // Ensure agent exists in database with these roles
    if let Err(e) = state.db.upsert_agent(Uuid::parse_str(&owner_id).unwrap(), agent_uuid, roles.clone()).await {
        tracing::warn!("Failed to upsert agent during token generation: {}", e);
    }
    
    Ok(Json(TokenResponse {
        token,
        owner_id,
        agent_id,
        roles,
        exp,
    }))
}

async fn metrics() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let mf = prometheus::gather();
    let mut buf = Vec::new();
    let _ = encoder.encode(&mf, &mut buf);
    ([("content-type", "text/plain; version=0.0.4")], buf)
}

static HTTP_REQ_TOTAL: StdOnceLock<IntCounterVec> = StdOnceLock::new();
static HTTP_REQ_HISTO: StdOnceLock<HistogramVec> = StdOnceLock::new();

async fn http_metrics_middleware(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let method = req.method().as_str().to_string();
    // Avoid exploding labels: keep path template-ish by trimming IDs
    let path_owned = req.uri().path().to_string();
    let path_label: String = if path_owned.len() > 64 { path_owned[..64].to_string() } else { path_owned.clone() };
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    let dur = start.elapsed().as_secs_f64();
    let counter = HTTP_REQ_TOTAL.get_or_init(|| register_int_counter_vec!(
        "http_requests_total","HTTP requests total", &["method","path","status"]
    ).unwrap());
    let histo = HTTP_REQ_HISTO.get_or_init(|| register_histogram_vec!(
        "http_request_duration_seconds","HTTP request duration seconds", &["method","path","status"],
        vec![0.005,0.01,0.025,0.05,0.1,0.25,0.5,1.0,2.5,5.0]
    ).unwrap());
    counter.with_label_values(&[&method, &path_label, &status]).inc();
    histo.with_label_values(&[&method, &path_label, &status]).observe(dur);
    resp
}

async fn openapi_spec() -> impl IntoResponse {
    let spec = include_str!("../../../docs/openapi.json");
    (
        axum::http::StatusCode::OK,
        [("content-type", "application/json")],
        spec,
    )
}

async fn docs_page() -> Html<&'static str> {
    Html(r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8"/>
    <title>RCRT API Docs</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>body { margin: 0; padding: 0; }</style>
    <script src="https://cdn.redoc.ly/redoc/latest/bundles/redoc.standalone.js"></script>
  </head>
  <body>
    <redoc spec-url="/openapi.json"></redoc>
  </body>
</html>"#)
}

async fn swagger_page() -> Html<&'static str> {
    Html(r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8"/>
    <title>RCRT Swagger</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
  </head>
  <body>
    <div id="swagger"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.ui = SwaggerUIBundle({
        url: '/openapi.json',
        dom_id: '#swagger',
        presets: [SwaggerUIBundle.presets.apis],
        layout: 'BaseLayout'
      });
    </script>
  </body>
</html>"#)
}

#[derive(Deserialize)]
struct CreateReq {
    title: String,
    description: Option<String>,        // NEW: Detailed description
    semantic_version: Option<String>,   // NEW: Semantic version
    context: serde_json::Value,
    tags: Vec<String>,
    schema_name: Option<String>,
    llm_hints: Option<serde_json::Value>, // NEW: Instance-level LLM hints
    visibility: Option<String>,
    sensitivity: Option<String>,
    ttl: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
struct CreateResp { id: Uuid }

async fn create_breadcrumb(State(state): State<AppState>, auth: AuthContext, headers: axum::http::HeaderMap, Json(req): Json<CreateReq>) -> Result<Json<CreateResp>, (axum::http::StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "emitter" || r == "curator") {
        return Err((StatusCode::FORBIDDEN, "emitter role required".into()));
    }
    if let Some(key) = headers.get("Idempotency-Key").and_then(|h| h.to_str().ok()) {
        if !state.db.record_idempotency(auth.owner_id, Some(auth.agent_id), key, "breadcrumb", None).await.map_err(internal_error)? {
            return Err((StatusCode::CONFLICT, "duplicate idempotency key".into()));
        }
    }
    // STEP 1: Extract text for embedding AND pointer extraction (do once)
    let llm_text = extract_text_for_embedding_struct(&req);
    
    // Try embedding before insert for atomicity if available
    let emb = embedding_policy::get_or_fallback_embedding(
        llm_text.clone(),
        req.schema_name.as_deref()
    );
    
    // STEP 2: POINTER EXTRACTION - Extract before moving req
    // Tags ARE pointers - extract semantic keywords for context assembly
    let tag_pointers: Vec<String> = req.tags.iter()
        .filter(|t| !t.contains(':'))  // No namespace prefix = pointer
        .filter(|t| !is_state_tag(t))  // Not lifecycle state
        .map(|t| t.to_lowercase())
        .collect();
    
    // Extract keywords from llm_hints-transformed text
    let extracted_pointers = extract_keywords_simple(&llm_text);
    
    // Combine hybrid pointers
    let mut all_pointers = tag_pointers;
    all_pointers.extend(extracted_pointers);
    all_pointers.sort();
    all_pointers.dedup();
    
    let entity_keywords_final = if all_pointers.is_empty() {
        None
    } else {
        Some(all_pointers)
    };
    
    // STEP 3: Apply automatic TTL policies for certain breadcrumb types
    let mut breadcrumb_create = BreadcrumbCreate {
        title: req.title.clone(),
        description: None,          // Will be set below
        semantic_version: None,     // Will be set below
        context: req.context.clone(),
        tags: req.tags.clone(),
        schema_name: req.schema_name.clone(),
        llm_hints: None,            // Will be set below
        visibility: req.visibility.and_then(|v| match v.as_str() {"public"=>Some(rcrt_core::models::Visibility::Public),"private"=>Some(rcrt_core::models::Visibility::Private),"team"=>Some(rcrt_core::models::Visibility::Team),_=>None}),
        sensitivity: req.sensitivity.and_then(|s| match s.as_str() {"pii"=>Some(rcrt_core::models::Sensitivity::Pii),"secret"=>Some(rcrt_core::models::Sensitivity::Secret),"low"=>Some(rcrt_core::models::Sensitivity::Low),_=>None}),
        ttl: req.ttl,
        entity_keywords: entity_keywords_final,  // Hybrid pointers
    };
    
    // Map new fields from request to BreadcrumbCreate
    breadcrumb_create.description = req.description;
    breadcrumb_create.semantic_version = req.semantic_version;
    breadcrumb_create.llm_hints = req.llm_hints;
    
    // Apply automatic TTL based on schema and tags
    hygiene::apply_auto_ttl(&mut breadcrumb_create, req.schema_name.as_deref(), &req.tags);
    
    let bc = state.db.create_breadcrumb_with_embedding_for(
        auth.owner_id,
        Some(auth.agent_id),
        Some(auth.agent_id),
        breadcrumb_create,
        emb
    ).await.map_err(internal_error)?;
    // Publish event (best-effort)
    #[cfg(feature = "nats")]
    if let Some(conn) = &state.nats_conn {
        tracing::info!("🔧 NATS: Publishing breadcrumb events for {}", bc.id);
        
        // Send both created and updated for immediate consumers that expect either
        let payload_base = json!({
            "breadcrumb_id": bc.id,
            "owner_id": auth.owner_id,
            "version": bc.version,
            "tags": bc.tags,
            "schema_name": bc.schema_name,
            "updated_at": bc.updated_at,
            "context": bc.context
        });
        let mut created_obj = payload_base.clone();
        if let Some(obj) = created_obj.as_object_mut() {
            obj.insert("type".to_string(), json!("breadcrumb.created"));
        }
        let mut updated_obj = payload_base;
        if let Some(obj) = updated_obj.as_object_mut() {
            obj.insert("type".to_string(), json!("breadcrumb.updated"));
        }
        let created = created_obj.to_string();
        let updated = updated_obj.to_string();
        let subj_created = format!("bc.{}.created", bc.id);
        let subj_updated = format!("bc.{}.updated", bc.id);
        
        tracing::info!("🔧 NATS: Publishing to {} and {}", subj_created, subj_updated);
        
        match conn.publish(&subj_created, created.as_bytes()) {
            Ok(_) => tracing::info!("🔧 NATS: ✅ Published created event"),
            Err(e) => tracing::error!("🔧 NATS: ❌ Failed to publish created event: {}", e)
        }
        
        match conn.publish(&subj_updated, updated.as_bytes()) {
            Ok(_) => tracing::info!("🔧 NATS: ✅ Published updated event"),
            Err(e) => tracing::error!("🔧 NATS: ❌ Failed to publish updated event: {}", e)
        }
        
        dispatch_webhooks(&state, auth.owner_id, &bc, &updated).await;
    } else {
        tracing::warn!("🔧 NATS: ❌ No NATS connection available - events will not be published!");
    }
    Ok(Json(CreateResp { id: bc.id }))
}

async fn get_breadcrumb_context(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<BreadcrumbContextView>, (axum::http::StatusCode, String)> {
    let Some(mut view) = state.db.get_breadcrumb_context_for(auth.owner_id, Some(auth.agent_id), id).await.map_err(internal_error)? else {
        return Err((axum::http::StatusCode::NOT_FOUND, "not found".into()));
    };
    
    // Usage-based TTL tracking removed (deprecated)
    // let _ = sqlx::query("...")
    
    // ONLY use instance llm_hints - no fallback!
    let final_hints = view.llm_hints.clone()
        .and_then(|v| serde_json::from_value::<transforms::LlmHints>(v).ok());
    
    // Apply hints if we found any (if no llm_hints, return untransformed)
    if let Some(hints) = final_hints {
        let engine = transforms::TransformEngine::new();
        match engine.apply_llm_hints(&view.context, &hints) {
            Ok(transformed) => {
                tracing::debug!("Applied llm_hints transform for breadcrumb {} (schema: {:?})", id, view.schema_name);
                view.context = transformed;
            }
            Err(e) => {
                tracing::warn!("llm_hints transform failed for breadcrumb {} (schema: {:?}): {}. Returning untransformed.", id, view.schema_name, e);
                // Continue with untransformed context - not fatal
            }
        }
    }
    
    Ok(Json(view))
}

async fn get_breadcrumb_full(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<BreadcrumbFull>, (StatusCode, String)> {
    // Access control handled by get_breadcrumb_full_for (checks visibility and ACLs)
    let Some(full) = state.db.get_breadcrumb_full_for(auth.owner_id, Some(auth.agent_id), id).await.map_err(internal_error)? else {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    };
    Ok(Json(full))
}

#[derive(Deserialize)]
struct UpdateReq {
    title: Option<String>,
    description: Option<String>,        // NEW: Update description
    semantic_version: Option<String>,   // NEW: Update semantic version
    context: Option<serde_json::Value>,
    tags: Option<Vec<String>>,
    schema_name: Option<String>,
    llm_hints: Option<serde_json::Value>, // NEW: Update LLM hints
    visibility: Option<String>,
    sensitivity: Option<String>,
    ttl: Option<chrono::DateTime<chrono::Utc>>,
}

async fn update_breadcrumb(State(state): State<AppState>, auth: AuthContext, headers: axum::http::HeaderMap, axum::extract::Path(id): axum::extract::Path<Uuid>, Json(req): Json<UpdateReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    tracing::info!("🔧 UPDATE_BREADCRUMB DEBUG: Starting update for breadcrumb {}", id);
    tracing::info!("🔧 Agent: {}, Owner: {}", auth.agent_id, auth.owner_id);
    
    let expected_version = headers.get(axum::http::header::IF_MATCH).and_then(|h| h.to_str().ok()).and_then(|s| s.trim_matches('"').parse::<i32>().ok());
    tracing::info!("🔧 Expected version: {:?}", expected_version);
    tracing::info!("🔧 Request payload: title={:?}, context_exists={}, tags={:?}", 
        req.title, req.context.is_some(), req.tags);
    
    if let Some(context) = &req.context {
        let context_preview = serde_json::to_string(context).unwrap_or_default();
        let preview = if context_preview.len() > 200 { 
            format!("{}...", &context_preview[..200]) 
        } else { 
            context_preview 
        };
        tracing::info!("🔧 Context payload preview: {}", preview);
    }
    
    let upd = rcrt_core::models::BreadcrumbUpdate {
        title: req.title,
        description: req.description,
        semantic_version: req.semantic_version,
        context: req.context,
        tags: req.tags,
        schema_name: req.schema_name,
        llm_hints: req.llm_hints,
        visibility: req.visibility.and_then(|v| match v.as_str() {"public"=>Some(rcrt_core::models::Visibility::Public),"private"=>Some(rcrt_core::models::Visibility::Private),"team"=>Some(rcrt_core::models::Visibility::Team),_=>None}),
        sensitivity: req.sensitivity.and_then(|s| match s.as_str() {"pii"=>Some(rcrt_core::models::Sensitivity::Pii),"secret"=>Some(rcrt_core::models::Sensitivity::Secret),"low"=>Some(rcrt_core::models::Sensitivity::Low),_=>None}),
        ttl: req.ttl,
    };
    
    tracing::info!("🔧 BreadcrumbUpdate created: context_is_some={}", upd.context.is_some());
    
    let bc = state.db.update_breadcrumb(auth.owner_id, auth.agent_id, id, expected_version, upd).await.map_err(|e| {
        tracing::error!("🔧 Database update failed: {}", e);
        if e.to_string().contains("version_mismatch") { (StatusCode::PRECONDITION_FAILED, e.to_string()) } else { internal_error(e) }
    })?;
    
    tracing::info!("🔧 Database update succeeded: version={}, context_preview={}", 
        bc.version, 
        serde_json::to_string(&bc.context).unwrap_or_default().chars().take(100).collect::<String>()
    );
    
    // Publish update events (same as create!)
    #[cfg(feature = "nats")]
    if let Some(conn) = &state.nats_conn {
        let payload = json!({
            "type": "breadcrumb.updated",
            "breadcrumb_id": bc.id,
            "owner_id": auth.owner_id,
            "version": bc.version,
            "tags": bc.tags,
            "schema_name": bc.schema_name,
            "updated_at": bc.updated_at,
            "context": bc.context
        });
        let updated = payload.to_string();
        let subj_updated = format!("bc.{}.updated", bc.id);
        
        tracing::info!("🔧 NATS: Publishing update event for {}", bc.id);
        let _ = conn.publish(&subj_updated, updated.as_bytes());
        
        dispatch_webhooks(&state, auth.owner_id, &bc, &updated).await;
    }
    
    Ok(Json(json!({"ok": true})))
}

async fn delete_breadcrumb(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let rows = state.db.delete_breadcrumb(auth.owner_id, auth.agent_id, id).await.map_err(internal_error)?;
    if rows == 0 { return Err((StatusCode::NOT_FOUND, "not found".into())); }
    Ok(Json(json!({"ok": true})))
}

// ============ LLM-FRIENDLY API ENDPOINTS ============

#[derive(Deserialize)]
struct TagOperation {
    tags: Vec<String>,
}

async fn add_tags(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<TagOperation>
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") {
        return Err((StatusCode::FORBIDDEN, "curator or emitter role required".into()));
    }
    
    // Atomic tag addition with deduplication
    let result = sqlx::query("
        UPDATE breadcrumbs 
        SET tags = (
            SELECT array_agg(DISTINCT tag ORDER BY tag)
            FROM unnest(tags || $1::text[]) AS tag
        ),
        version = version + 1,
        updated_at = NOW(),
        updated_by = $3
        WHERE id = $2 
        AND owner_id = $4
        RETURNING version
    ")
    .bind(&req.tags)
    .bind(id)
    .bind(auth.agent_id)
    .bind(auth.owner_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal_error)?;
    
    if result.is_none() {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    
    // Publish update event
    #[cfg(feature = "nats")]
    {
        let _ = state.nats_conn.as_ref().map(|nc| {
            let evt = json!({
                "type": "breadcrumb.updated",
                "breadcrumb_id": id,
                "owner_id": auth.owner_id,
                "tags_added": req.tags.len()
            });
            nc.publish(&format!("bc.{}.updated", id), evt.to_string().as_bytes())
        });
    }
    
    tracing::info!("Added {} tags to breadcrumb {}", req.tags.len(), id);
    Ok(Json(json!({"ok": true})))
}

async fn remove_tags(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<TagOperation>
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") {
        return Err((StatusCode::FORBIDDEN, "curator or emitter role required".into()));
    }
    
    // Atomic tag removal
    let result = sqlx::query("
        UPDATE breadcrumbs 
        SET tags = (
            SELECT array_agg(tag ORDER BY tag)
            FROM unnest(tags) AS tag
            WHERE tag != ALL($1::text[])
        ),
        version = version + 1,
        updated_at = NOW(),
        updated_by = $3
        WHERE id = $2 
        AND owner_id = $4
        RETURNING version
    ")
    .bind(&req.tags)
    .bind(id)
    .bind(auth.agent_id)
    .bind(auth.owner_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal_error)?;
    
    if result.is_none() {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    
    // Publish update event
    #[cfg(feature = "nats")]
    {
        let _ = state.nats_conn.as_ref().map(|nc| {
            let evt = json!({
                "type": "breadcrumb.updated",
                "breadcrumb_id": id,
                "owner_id": auth.owner_id,
                "tags_removed": req.tags.len()
            });
            nc.publish(&format!("bc.{}.updated", id), evt.to_string().as_bytes())
        });
    }
    
    tracing::info!("Removed {} tags from breadcrumb {}", req.tags.len(), id);
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ContextMergeReq {
    context: serde_json::Value,
}

async fn merge_context(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<ContextMergeReq>
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") {
        return Err((StatusCode::FORBIDDEN, "curator or emitter role required".into()));
    }
    
    // Deep merge: new values override, missing fields preserved
    let result = sqlx::query("
        UPDATE breadcrumbs 
        SET context = jsonb_deep_merge(context, $1::jsonb),
            version = version + 1,
            updated_at = NOW(),
            updated_by = $3
        WHERE id = $2 
        AND owner_id = $4
        RETURNING version
    ")
    .bind(&req.context)
    .bind(id)
    .bind(auth.agent_id)
    .bind(auth.owner_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal_error)?;
    
    if result.is_none() {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    
    // Publish update event
    #[cfg(feature = "nats")]
    {
        let _ = state.nats_conn.as_ref().map(|nc| {
            let evt = json!({
                "type": "breadcrumb.updated",
                "breadcrumb_id": id,
                "owner_id": auth.owner_id,
                "context_merged": true
            });
            nc.publish(&format!("bc.{}.updated", id), evt.to_string().as_bytes())
        });
    }
    
    tracing::info!("Merged context for breadcrumb {}", id);
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ApprovalRequest {
    reason: Option<String>,
}

async fn approve_breadcrumb(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<ApprovalRequest>
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") {
        return Err((StatusCode::FORBIDDEN, "curator or emitter role required".into()));
    }
    
    // Semantic action: Add approved + validated tags, remove draft/pending
    let result = sqlx::query("
        UPDATE breadcrumbs 
        SET tags = (
            SELECT array_agg(DISTINCT tag ORDER BY tag)
            FROM unnest(
                array_remove(
                    array_remove(tags, 'draft'), 
                    'pending'
                ) || ARRAY['approved', 'validated']
            ) AS tag
        ),
        version = version + 1,
        updated_at = NOW(),
        updated_by = $2
        WHERE id = $1 
        AND owner_id = $3
        RETURNING version
    ")
    .bind(id)
    .bind(auth.agent_id)
    .bind(auth.owner_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal_error)?;
    
    if result.is_none() {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    
    // Publish update event
    #[cfg(feature = "nats")]
    {
        let _ = state.nats_conn.as_ref().map(|nc| {
            let evt = json!({
                "type": "breadcrumb.updated",
                "breadcrumb_id": id,
                "owner_id": auth.owner_id,
                "action": "approved",
                "reason": req.reason
            });
            nc.publish(&format!("bc.{}.updated", id), evt.to_string().as_bytes())
        });
    }
    
    tracing::info!("Approved breadcrumb {} - reason: {:?}", id, req.reason);
    Ok(Json(json!({"ok": true})))
}

async fn reject_breadcrumb(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<ApprovalRequest>
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") {
        return Err((StatusCode::FORBIDDEN, "curator or emitter role required".into()));
    }
    
    // Semantic action: Add rejected tag, remove approved/validated/draft
    let result = sqlx::query("
        UPDATE breadcrumbs 
        SET tags = (
            SELECT array_agg(DISTINCT tag ORDER BY tag)
            FROM unnest(
                array_remove(
                    array_remove(
                        array_remove(tags, 'approved'),
                        'validated'
                    ),
                    'draft'
                ) || ARRAY['rejected']
            ) AS tag
        ),
        version = version + 1,
        updated_at = NOW(),
        updated_by = $2
        WHERE id = $1 
        AND owner_id = $3
        RETURNING version
    ")
    .bind(id)
    .bind(auth.agent_id)
    .bind(auth.owner_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal_error)?;
    
    if result.is_none() {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    
    // Publish update event
    #[cfg(feature = "nats")]
    {
        let _ = state.nats_conn.as_ref().map(|nc| {
            let evt = json!({
                "type": "breadcrumb.updated",
                "breadcrumb_id": id,
                "owner_id": auth.owner_id,
                "action": "rejected",
                "reason": req.reason
            });
            nc.publish(&format!("bc.{}.updated", id), evt.to_string().as_bytes())
        });
    }
    
    tracing::info!("Rejected breadcrumb {} - reason: {:?}", id, req.reason);
    Ok(Json(json!({"ok": true})))
}

async fn admin_purge(State(state): State<AppState>, auth: AuthContext) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    
    tracing::info!("Admin purge triggered by agent: {}", auth.agent_id);
    
    // Run comprehensive cleanup
    let ttl_purged = state.db.purge_expired_for_owner(auth.owner_id).await.map_err(internal_error)?;
    
    let health_checks_purged = hygiene::cleanup_health_checks(&state.db)
        .await
        .map_err(internal_error)?;
    
    let expired_purged = hygiene::cleanup_expired_breadcrumbs(&state.db)
        .await
        .map_err(internal_error)?;
    
    let total_purged = ttl_purged + (health_checks_purged as i64) + (expired_purged as i64);
    
    tracing::info!("Admin purge completed: {} breadcrumbs purged", total_purged);
    
    Ok(Json(json!({
        "purged": total_purged,
        "ttl_purged": ttl_purged,
        "health_checks_purged": health_checks_purged,
        "expired_purged": expired_purged
    })))
}

async fn get_breadcrumb_history(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let rows = state.db.list_breadcrumb_history(auth.owner_id, Some(auth.agent_id), id).await.map_err(internal_error)?;
    let out = rows.into_iter().map(|(v,c,u,b)| json!({"version": v, "context": c, "updated_at": u, "updated_by": b})).collect();
    Ok(Json(out))
}

async fn grant_acl(State(state): State<AppState>, auth: AuthContext, Json(req): Json<AclGrantAgent>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    let id = state.db.grant_acl_agent(auth.owner_id, req.breadcrumb_id, req.grantee_agent_id, &req.action).await.map_err(internal_error)?;
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct AclRevokeReq { breadcrumb_id: Uuid, grantee_agent_id: Uuid, action: String }
async fn revoke_acl(State(state): State<AppState>, auth: AuthContext, Json(req): Json<AclRevokeReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    let rows = state.db.revoke_acl_agent(auth.owner_id, req.breadcrumb_id, req.grantee_agent_id, &req.action).await.map_err(internal_error)?;
    Ok(Json(json!({"rows": rows})))
}

async fn list_acls(State(state): State<AppState>, auth: AuthContext) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let acls = state.db.list_acls(auth.owner_id).await.map_err(internal_error)?;
    let out = acls.into_iter().map(|(id, breadcrumb_id, grantee_agent_id, actions, created_at)| {
        json!({
            "id": id,
            "breadcrumb_id": breadcrumb_id,
            "grantee_agent_id": grantee_agent_id,
            "actions": actions,
            "created_at": created_at
        })
    }).collect();
    Ok(Json(out))
}

async fn dispatch_webhooks(state: &AppState, owner_id: Uuid, _bc: &rcrt_core::models::Breadcrumb, payload: &str) {
    // RCRT uses global broadcast + client-side tag matching
    // No server-side filtering via selector_subscriptions needed
    // This function only handles webhook dispatch for external integrations
    
    // Get all agents for this owner (webhooks can be configured per-agent)
    let Ok(agents) = state.db.list_agents(owner_id).await else { return; };
    
    // Dispatch webhooks for each agent that has them configured
    // list_agents returns Vec<(agent_id, roles, created_at)>
    for (agent_id, _roles, _created_at) in agents {
        if let Ok(hooks) = state.db.list_agent_webhooks(owner_id, agent_id).await {
            let secret = state.db.get_agent_webhook_secret(owner_id, agent_id).await.ok().flatten();
            for (_id, url) in hooks {
                let db = state.db.clone();
                let payload_str = payload.to_string();
                tokio::spawn(dispatch_webhook(db, owner_id, agent_id, url, payload_str, secret.clone()));
            }
        }
    }
}

static WEBHOOK_RESULTS: StdOnceLock<IntCounterVec> = StdOnceLock::new();
static WEBHOOK_DURATION: StdOnceLock<HistogramVec> = StdOnceLock::new();

async fn dispatch_webhook(db: Db, owner_id: Uuid, agent_id: Uuid, url: String, body: String, secret: Option<String>) {
    let client = HttpClient::new();
    let max_retries: usize = std::env::var("WEBHOOK_MAX_RETRIES").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let mut attempt: usize = 0;
    let counter = WEBHOOK_RESULTS.get_or_init(|| register_int_counter_vec!("webhook_delivery_total", "Webhook delivery results", & ["result"]).unwrap());
    let histo = WEBHOOK_DURATION.get_or_init(|| register_histogram_vec!(
        "webhook_delivery_duration_seconds","Webhook delivery duration seconds", &["result"],
        vec![0.05,0.1,0.25,0.5,1.0,2.5,5.0]
    ).unwrap());
    let all_start = std::time::Instant::now();
    loop {
        let mut req = client.post(&url).header("content-type", "application/json");
        if let Some(sec) = &secret {
            let mut mac = Hmac::<Sha256>::new_from_slice(sec.as_bytes()).unwrap();
            mac.update(body.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            req = req.header("X-RCRT-Signature", format!("sha256={}", sig));
        }
        let res = req.body(body.clone()).send().await;
        let ok = res.as_ref().map(|r| r.status().is_success()).unwrap_or(false);
        if ok { counter.with_label_values(&["success"]).inc(); histo.with_label_values(&["success"]).observe(all_start.elapsed().as_secs_f64()); break; }
        attempt += 1;
        if attempt >= max_retries {
            counter.with_label_values(&["failed"]).inc();
            histo.with_label_values(&["failed"]).observe(all_start.elapsed().as_secs_f64());
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                let err = res.err().map(|e| e.to_string()).unwrap_or_else(|| "non-2xx".into());
                let _ = db.enqueue_webhook_dlq(owner_id, agent_id, &url, &val, &err).await;
            }
            break;
        }
        let backoff_ms = (1u64 << attempt.min(6)) * 250; // capped exponential backoff
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
    }
}

#[derive(Deserialize)]
struct WebhookReq { url: String }
async fn register_webhook(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>, Json(req): Json<WebhookReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.agent_id != agent_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    let id = state.db.create_agent_webhook(auth.owner_id, agent_id, &req.url).await.map_err(internal_error)?;
    Ok(Json(json!({"id": id})))
}

async fn list_webhooks(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    if auth.agent_id != agent_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    let rows = state.db.list_agent_webhooks(auth.owner_id, agent_id).await.map_err(internal_error)?;
    let out = rows.into_iter().map(|(id, url)| json!({"id": id, "url": url})).collect();
    Ok(Json(out))
}

async fn deactivate_webhook(State(state): State<AppState>, auth: AuthContext, axum::extract::Path((agent_id, wid)): axum::extract::Path<(Uuid, Uuid)>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.agent_id != agent_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    let rows = state.db.deactivate_agent_webhook(auth.owner_id, agent_id, wid).await.map_err(internal_error)?;
    Ok(Json(json!({"rows": rows})))
}

#[derive(Deserialize)]
struct AgentRegReq { roles: Vec<String> }
async fn register_agent(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>, Json(req): Json<AgentRegReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.agent_id != agent_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    state.db.upsert_agent(auth.owner_id, agent_id, req.roles).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct SecretReq { secret: String }
async fn set_agent_secret(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>, Json(req): Json<SecretReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.agent_id != agent_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    state.db.set_agent_webhook_secret(auth.owner_id, agent_id, &req.secret).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

async fn list_agents(State(state): State<AppState>, auth: AuthContext) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let agents = state.db.list_agents(auth.owner_id).await.map_err(internal_error)?;
    let out = agents.into_iter().map(|(id, roles, created_at)| {
        json!({
            "id": id,
            "roles": roles,
            "created_at": created_at
        })
    }).collect();
    Ok(Json(out))
}

async fn get_agent(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let agent = state.db.get_agent(auth.owner_id, agent_id).await.map_err(internal_error)?;
    match agent {
        Some((id, roles, created_at)) => Ok(Json(json!({
            "id": id,
            "roles": roles,
            "created_at": created_at
        }))),
        None => Err((StatusCode::NOT_FOUND, "agent not found".into()))
    }
}

async fn delete_agent(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(agent_id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    state.db.delete_agent(auth.owner_id, agent_id).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct SecretCreateReq { name: String, scope_type: String, scope_id: Option<Uuid>, value: String }
async fn create_secret(State(state): State<AppState>, auth: AuthContext, Json(req): Json<SecretCreateReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    // Local KEK: read from env (base64)
    let kek_b64 = std::env::var("LOCAL_KEK_BASE64").map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "LOCAL_KEK_BASE64 missing".into()))?;
    let kek = base64::decode(kek_b64).map_err(internal_error)?;
    // Generate random DEK
    let dek = rand::random::<[u8;32]>();
    // Encrypt value with DEK using AES-GCM (real encryption, no placeholders)
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore, KeyInit};
    let key = Key::<Aes256Gcm>::from_slice(&dek);
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8;12]; OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, req.value.as_bytes()).map_err(internal_error)?;
    let mut enc_blob = Vec::with_capacity(12 + ciphertext.len());
    enc_blob.extend_from_slice(&nonce_bytes);
    enc_blob.extend_from_slice(&ciphertext);
    // Wrap DEK with KEK using XChaCha20-Poly1305 (libsodium style) for local demo
    use chacha20poly1305::{XChaCha20Poly1305, Key as XKey, XNonce};
    use chacha20poly1305::aead::KeyInit as _;
    let xkey = XKey::from_slice(&kek);
    let x = XChaCha20Poly1305::new(xkey);
    let mut xnonce_bytes = [0u8;24]; OsRng.fill_bytes(&mut xnonce_bytes);
    let dek_ct = x.encrypt(XNonce::from_slice(&xnonce_bytes), dek.as_slice()).map_err(internal_error)?;
    let mut dek_encrypted = Vec::with_capacity(24 + dek_ct.len());
    dek_encrypted.extend_from_slice(&xnonce_bytes);
    dek_encrypted.extend_from_slice(&dek_ct);
    let secret_id = state.db.create_secret(auth.owner_id, &req.name, &req.scope_type, req.scope_id, &enc_blob, &dek_encrypted, "local-keK").await.map_err(internal_error)?;
    Ok(Json(json!({"id": secret_id})))
}

#[derive(Deserialize)]
struct SecretDecryptReq { reason: Option<String> }
async fn decrypt_secret(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(secret_id): axum::extract::Path<Uuid>, Json(req): Json<SecretDecryptReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Fetch secret materials
    let Some((enc_blob, dek_wrapped, _kek_id)) = state.db.get_secret_material(auth.owner_id, secret_id).await.map_err(internal_error)? else {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    };
    // Unwrap DEK with local KEK
    let kek_b64 = std::env::var("LOCAL_KEK_BASE64").map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "LOCAL_KEK_BASE64 missing".into()))?;
    let kek = base64::decode(kek_b64).map_err(internal_error)?;
    use chacha20poly1305::{XChaCha20Poly1305, Key as XKey, XNonce, aead::Aead};
    use chacha20poly1305::aead::KeyInit as _;
    let x = XChaCha20Poly1305::new(XKey::from_slice(&kek));
    let (xnonce, dek_ct) = dek_wrapped.split_at(24);
    let dek = x.decrypt(XNonce::from_slice(xnonce), dek_ct).map_err(internal_error)?;
    // Decrypt value
    use aes_gcm::{Aes256Gcm, Key, Nonce, aead::Aead as Aead2};
    use aes_gcm::aead::KeyInit as _;
    let (nonce_bytes, ct) = enc_blob.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
    let plaintext = cipher.decrypt(Nonce::from_slice(nonce_bytes), ct).map_err(internal_error)?;
    state.db.audit_secret(secret_id, Some(auth.agent_id), "decrypt", req.reason.as_deref()).await.map_err(internal_error)?;
    Ok(Json(json!({"value": String::from_utf8_lossy(&plaintext)})))
}

async fn list_secrets(State(state): State<AppState>, auth: AuthContext, Query(q): Query<ListSecretsQuery>) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    // List secrets for the authenticated owner, optionally filtered by scope
    let rows = state.db.list_secrets(auth.owner_id, q.scope_type.as_deref(), q.scope_id).await.map_err(internal_error)?;
    let out = rows.into_iter().map(|(id, name, scope_type, scope_id, created_at)| {
        json!({
            "id": id,
            "name": name,
            "scope_type": scope_type,
            "scope_id": scope_id,
            "created_at": created_at
        })
    }).collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ListSecretsQuery { 
    scope_type: Option<String>, 
    scope_id: Option<Uuid> 
}

async fn update_secret(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(secret_id): axum::extract::Path<Uuid>, Json(req): Json<SecretUpdateReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { 
        return Err((StatusCode::FORBIDDEN, "curator role required".into())); 
    }
    
    // Re-encrypt with new value
    let kek_b64 = std::env::var("LOCAL_KEK_BASE64").map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "LOCAL_KEK_BASE64 missing".into()))?;
    let kek = base64::decode(kek_b64).map_err(internal_error)?;
    
    // Generate new DEK for the updated value
    let dek = rand::random::<[u8;32]>();
    
    // Encrypt new value with DEK
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore, KeyInit};
    let key = Key::<Aes256Gcm>::from_slice(&dek);
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8;12]; 
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, req.value.as_bytes()).map_err(internal_error)?;
    let mut enc_blob = Vec::with_capacity(12 + ciphertext.len());
    enc_blob.extend_from_slice(&nonce_bytes);
    enc_blob.extend_from_slice(&ciphertext);
    
    // Wrap DEK with KEK
    use chacha20poly1305::{XChaCha20Poly1305, Key as XKey, XNonce};
    use chacha20poly1305::aead::KeyInit as _;
    let xkey = XKey::from_slice(&kek);
    let x = XChaCha20Poly1305::new(xkey);
    let mut xnonce_bytes = [0u8;24]; 
    OsRng.fill_bytes(&mut xnonce_bytes);
    let dek_ct = x.encrypt(XNonce::from_slice(&xnonce_bytes), dek.as_slice()).map_err(internal_error)?;
    let mut dek_encrypted = Vec::with_capacity(24 + dek_ct.len());
    dek_encrypted.extend_from_slice(&xnonce_bytes);
    dek_encrypted.extend_from_slice(&dek_ct);
    
    // Update in database
    state.db.update_secret(auth.owner_id, secret_id, &enc_blob, &dek_encrypted).await.map_err(internal_error)?;
    state.db.audit_secret(secret_id, Some(auth.agent_id), "update", Some("value updated")).await.map_err(internal_error)?;
    
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct SecretUpdateReq { 
    value: String 
}

async fn delete_secret(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(secret_id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { 
        return Err((StatusCode::FORBIDDEN, "curator role required".into())); 
    }
    
    // Audit before deletion
    state.db.audit_secret(secret_id, Some(auth.agent_id), "delete", Some("secret deleted")).await.map_err(internal_error)?;
    
    // Delete the secret
    let rows = state.db.delete_secret(auth.owner_id, secret_id).await.map_err(internal_error)?;
    if rows == 0 { 
        return Err((StatusCode::NOT_FOUND, "secret not found".into())); 
    }
    
    Ok(Json(json!({"ok": true})))
}

async fn list_dlq(State(state): State<AppState>, auth: AuthContext) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    let rows = state.db.list_webhook_dlq(auth.owner_id).await.map_err(internal_error)?;
    let out = rows.into_iter().map(|(id, agent_id, url, payload, last_error, created_at)| json!({"id": id, "agent_id": agent_id, "url": url, "payload": payload, "last_error": last_error, "created_at": created_at})).collect();
    Ok(Json(out))
}

async fn retry_dlq(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    let Some((dlq_id, agent_id, url, payload)) = state.db.get_webhook_dlq(auth.owner_id, id).await.map_err(internal_error)? else {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    };
    let secret = state.db.get_agent_webhook_secret(auth.owner_id, agent_id).await.map_err(internal_error)?;
    let db = state.db.clone();
    tokio::spawn(dispatch_webhook(db, auth.owner_id, agent_id, url.clone(), payload.to_string(), secret));
    let _ = state.db.delete_webhook_dlq(auth.owner_id, dlq_id).await;
    Ok(Json(json!({"requeued": true})))
}

async fn delete_dlq(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    state.db.delete_webhook_dlq(auth.owner_id, id).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct TenantReq { name: String }
async fn ensure_tenant(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(tenant_id): axum::extract::Path<Uuid>, Json(req): Json<TenantReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.owner_id != tenant_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    state.db.ensure_tenant(tenant_id, &req.name).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

async fn list_tenants(State(state): State<AppState>, auth: AuthContext) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    let tenants = state.db.list_tenants().await.map_err(internal_error)?;
    let out = tenants.into_iter().map(|(id, name, created_at)| {
        json!({
            "id": id,
            "name": name,
            "created_at": created_at
        })
    }).collect();
    Ok(Json(out))
}

async fn get_tenant(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(tenant_id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let tenant = state.db.get_tenant(tenant_id).await.map_err(internal_error)?;
    match tenant {
        Some((id, name, created_at)) => Ok(Json(json!({
            "id": id,
            "name": name,
            "created_at": created_at
        }))),
        None => Err((StatusCode::NOT_FOUND, "tenant not found".into()))
    }
}

async fn update_tenant(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(tenant_id): axum::extract::Path<Uuid>, Json(req): Json<TenantReq>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if auth.owner_id != tenant_id && !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }
    state.db.update_tenant(tenant_id, &req.name).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_tenant(State(state): State<AppState>, auth: AuthContext, axum::extract::Path(tenant_id): axum::extract::Path<Uuid>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth.roles.iter().any(|r| r == "curator") { return Err((StatusCode::FORBIDDEN, "curator role required".into())); }
    state.db.delete_tenant(tenant_id).await.map_err(internal_error)?;
    Ok(Json(json!({"ok": true})))
}

fn internal_error<E: std::fmt::Display>(e: E) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn openrouter_chat(
    client: &HttpClient,
    api_key: &str,
    referer: Option<&str>,
    site_title: Option<&str>,
    model: &str,
    role_system: String,
    user_messages: serde_json::Value,
) -> Result<String, (StatusCode, String)> {
    let base_url = "https://openrouter.ai/api/v1/chat/completions";
    let sys_msg = serde_json::json!({"role":"system","content": role_system});
    let merged: serde_json::Value = match user_messages {
        serde_json::Value::Array(arr) => {
            let mut msgs = vec![sys_msg];
            msgs.extend(arr);
            serde_json::Value::Array(msgs)
        }
        other => serde_json::json!([sys_msg, {"role":"user","content": other}])
    };
    let mut req = client.post(base_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json");
    if let Some(r) = referer { req = req.header("HTTP-Referer", r); }
    if let Some(t) = site_title { req = req.header("X-Title", t); }
    let payload = serde_json::json!({
        "model": model,
        "messages": merged,
        "stream": false
    });
    let resp = req.json(&payload).send().await.map_err(internal_error)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err((StatusCode::BAD_GATEWAY, format!("openrouter {}: {}", status, body)));
    }
    let v: serde_json::Value = resp.json().await.map_err(internal_error)?;
    let content = v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or((StatusCode::BAD_GATEWAY, "invalid openrouter response".into()))?;
    Ok(content.to_string())
}

#[derive(Deserialize)]
struct AgentRunInput {
    model: String,
    messages: serde_json::Value,
    referer: Option<String>,
    site_title: Option<String>,
}

#[derive(Serialize)]
struct AgentRunOutput {
    agent1_plan: String,
    agent2_execution: String,
    agent3_summary: String,
    final_answer: String,
}

async fn run_agents(State(_state): State<AppState>, auth: AuthContext, Json(body): Json<AgentRunInput>) -> Result<Json<AgentRunOutput>, (StatusCode, String)> {
    // Require curator or emitter to invoke multi-agent orchestration
    if !auth.roles.iter().any(|r| r == "curator" || r == "emitter") { return Err((StatusCode::FORBIDDEN, "forbidden".into())); }

    let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "OPENROUTER_API_KEY missing".into()))?;
    let client = HttpClient::new();
    let referer = body.referer.or_else(|| std::env::var("OPENROUTER_REFERER").ok());
    let site_title = body.site_title.or_else(|| std::env::var("OPENROUTER_SITE_TITLE").ok());

    // Three simple roles
    let agent1_plan = openrouter_chat(
        &client,
        &api_key,
        referer.as_deref(),
        site_title.as_deref(),
        &body.model,
        "You are Planner. Draft a concise plan. Do not execute, only plan.".to_string(),
        body.messages.clone()
    ).await?;

    let agent2_execution = openrouter_chat(
        &client,
        &api_key,
        referer.as_deref(),
        site_title.as_deref(),
        &body.model,
        format!("You are Researcher. Execute the plan strictly and produce findings. Plan:\n{}", agent1_plan),
        body.messages.clone()
    ).await?;

    let agent3_summary = openrouter_chat(
        &client,
        &api_key,
        referer.as_deref(),
        site_title.as_deref(),
        &body.model,
        format!("You are Synthesizer. Summarize findings into a direct answer. Findings:\n{}", agent2_execution),
        body.messages.clone()
    ).await?;

    let final_answer = agent3_summary.clone();
    Ok(Json(AgentRunOutput { agent1_plan, agent2_execution, agent3_summary, final_answer }))
}

#[derive(Deserialize)]
struct ListQuery { tag: Option<String>, schema_name: Option<String>, limit: Option<i64>, offset: Option<i64>, include_context: Option<bool> }

#[derive(Serialize)]
struct ListItem { id: Uuid, title: String, tags: Vec<String>, schema_name: Option<String>, version: i32, updated_at: chrono::DateTime<chrono::Utc> }

#[derive(Serialize)]
#[serde(untagged)]
enum ListResult {
    List(Vec<ListItem>),
    Context(Vec<BreadcrumbContextView>),
}

async fn list_breadcrumbs(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Result<Json<ListResult>, (axum::http::StatusCode, String)> {
    let include_context = q.include_context.unwrap_or(false);
    
    let mut sql = if include_context {
        String::from("select id, title, context, tags, schema_name, version, updated_at from breadcrumbs")
    } else {
        String::from("select id, title, tags, schema_name, version, updated_at from breadcrumbs")
    };
    
    let mut conditions = Vec::new();
    let mut bind_idx = 1;
    
    if q.tag.is_some() {
        conditions.push(format!("${} = any(tags)", bind_idx));
        bind_idx += 1;
    }
    if q.schema_name.is_some() {
        conditions.push(format!("schema_name = ${}", bind_idx));
        bind_idx += 1;
    }
    
    if !conditions.is_empty() {
        sql.push_str(" where ");
        sql.push_str(&conditions.join(" and "));
    }
    
    sql.push_str(" order by updated_at desc");
    if let Some(limit) = q.limit { sql.push_str(&format!(" limit {}", limit.max(1))); }
    if let Some(offset) = q.offset { sql.push_str(&format!(" offset {}", offset.max(0))); }

    if include_context {
        let rows = match (q.tag.as_ref(), q.schema_name.as_ref()) {
            (Some(tag), Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(tag)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (Some(tag), None) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(tag)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, None) => {
                sqlx::query_as::<_, (Uuid,String,serde_json::Value,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            }
        };
        let items = rows.into_iter().map(|(id,title,context,tags,schema_name,version,updated_at)| {
            BreadcrumbContextView { 
                id, title, description: None, semantic_version: None, context, tags, schema_name, llm_hints: None, version, updated_at 
            }
        }).collect();
        Ok(Json(ListResult::Context(items)))
    } else {
        let rows = match (q.tag.as_ref(), q.schema_name.as_ref()) {
            (Some(tag), Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(tag)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (Some(tag), None) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(tag)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, Some(schema)) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .bind(schema)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            },
            (None, None) => {
                sqlx::query_as::<_, (Uuid,String,Vec<String>,Option<String>,i32,chrono::DateTime<chrono::Utc>)>(&sql)
                    .fetch_all(&state.db.pool)
                    .await
                    .map_err(internal_error)?
            }
        };
        let items = rows.into_iter().map(|(id,title,tags,schema_name,version,updated_at)| ListItem{ id, title, tags, schema_name, version, updated_at }).collect();
        Ok(Json(ListResult::List(items)))
    }
}

#[derive(Deserialize)]
// Auth extractor: JWT by default; dev mode explicit via AUTH_MODE=disabled with OWNER_ID/AGENT_ID
#[derive(Clone, Debug)]
struct AuthContext { owner_id: Uuid, agent_id: Uuid, roles: Vec<String> }

#[derive(Debug, Deserialize)]
struct Claims { sub: String, owner_id: String, roles: Option<Vec<String>>, exp: Option<usize> }

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthContext {
    type Rejection = (StatusCode, String);
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // Dev mode
        let auth_mode = std::env::var("AUTH_MODE").unwrap_or_else(|_| "enabled".into());
        if auth_mode == "disabled" {
            let owner = std::env::var("OWNER_ID").ok().and_then(|s| Uuid::parse_str(&s).ok())
                .ok_or((StatusCode::UNAUTHORIZED, "OWNER_ID required in disabled auth mode".into()))?;
            let agent = std::env::var("AGENT_ID").ok().and_then(|s| Uuid::parse_str(&s).ok())
                .unwrap_or_else(|| Uuid::nil());
            return Ok(AuthContext { owner_id: owner, agent_id: agent, roles: vec!["curator".into(), "emitter".into(), "subscriber".into()] });
        }

        let Some(dec_key) = &state.jwt_decoding_key else {
            return Err((StatusCode::UNAUTHORIZED, "JWT required; set JWT_PUBLIC_KEY_PEM or use AUTH_MODE=disabled explicitly".into()));
        };
        // Allow Bearer header or access_token/token query for SSE/browser clients
        let token = if let Some(hv) = parts.headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()) {
            hv.strip_prefix("Bearer ").ok_or((StatusCode::UNAUTHORIZED, "invalid Authorization header".into()))?
                .to_string()
        } else {
            // Parse query for access_token
            let q = parts.uri.query().unwrap_or("");
            let mut token_val: Option<String> = None;
            for pair in q.split('&') {
                let mut it = pair.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    if k == "access_token" || k == "token" {
                        token_val = Some(percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string());
                        break;
                    }
                }
            }
            token_val.ok_or((StatusCode::UNAUTHORIZED, "missing Authorization".into()))?
        };
        let data = decode::<Claims>(&token, dec_key, &state.jwt_validation)
            .map_err(|e| (StatusCode::UNAUTHORIZED, format!("invalid token: {}", e)))?;
        let owner = Uuid::parse_str(&data.claims.owner_id)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid owner_id in token".into()))?;
        let agent = Uuid::parse_str(&data.claims.sub)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid sub in token".into()))?;
        let roles = data.claims.roles.unwrap_or_default();
        // Ensure agent row exists with roles so FK on created_by/updated_by succeeds
        if let Err(e) = state.db.upsert_agent(owner, agent, roles.clone()).await {
            return Err(internal_error(e));
        }
        Ok(AuthContext { owner_id: owner, agent_id: agent, roles })
    }
}

// SSE stream (NATS-backed when feature enabled)
#[cfg(feature = "nats")]
async fn sse_stream(State(state): State<AppState>, auth: AuthContext) -> Result<axum::response::Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>, StatusCode> {
    use axum::response::Sse;
    use axum::response::sse::Event;
    use tokio_stream::StreamExt;
    use tokio::sync::mpsc;
    use chrono::Utc;
    use std::time::Duration;
    if state.nats_conn.is_none() { 
        tracing::error!("🔧 SSE: ❌ No NATS connection available for SSE stream!");
        return Err(StatusCode::SERVICE_UNAVAILABLE); 
    }
    
    let conn = state.nats_conn.as_ref().unwrap().clone();
    tracing::info!("🔧 SSE: 📡 NEW SSE CONNECTION from agent {} (owner: {})", auth.agent_id, auth.owner_id);
    
    // Subscribe to global breadcrumb update events
    // RCRT uses tag-based routing with client-side filtering
    // No per-agent topics needed - global broadcast is simpler and scales better
    tracing::info!("🔧 SSE: Subscribing to NATS bc.*.updated (global broadcast)");
    let sub_bc = conn.subscribe("bc.*.updated").map_err(|e| {
        tracing::error!("🔧 SSE: ❌ Failed to subscribe to bc.*.updated: {}", e);
        StatusCode::SERVICE_UNAVAILABLE
    })?;
    
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tracing::info!("🔧 SSE: ✅ NATS subscriptions established, spawning bridge task...");

    // Spawn a bridge task
    let owner = auth.owner_id;
    let tx_bc = tx.clone();
    tokio::task::spawn_blocking(move || {
        tracing::info!("🔧 SSE: Bridge task started, listening for NATS bc.*.updated events...");
        for msg in sub_bc.messages() {
            if let Ok(txt) = std::str::from_utf8(&msg.data) {
                tracing::info!("🔧 SSE: 📡 NATS event received: {}", &txt[..std::cmp::min(100, txt.len())]);
                
                let pass = serde_json::from_str::<serde_json::Value>(txt)
                    .ok()
                    .and_then(|v| v.get("owner_id").cloned())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .map(|oid| {
                        let matches = oid == owner;
                        tracing::info!("🔧 SSE: Owner filter - event owner: {}, my owner: {}, matches: {}", oid, owner, matches);
                        matches
                    })
                    .unwrap_or(false);
                
                if pass { 
                    tracing::info!("🔧 SSE: ✅ Owner filter passed, forwarding event to SSE client");
                    let _ = tx_bc.send(txt.to_string()); 
                } else {
                    tracing::info!("🔧 SSE: ⏭️ Owner filter failed, skipping event");
                }
            } else {
                tracing::warn!("🔧 SSE: ⚠️ Failed to decode NATS message as UTF-8");
            }
        }
    });

    // Heartbeat pings every 5s so clients know the stream is alive
    let tx_ping = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let ping = serde_json::json!({"type":"ping","ts": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)}).to_string();
            let _ = tx_ping.send(ping);
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        .map(|data| Ok(Event::default().data(data)));
    Ok(Sse::new(stream))
}

// SSE endpoint unavailable when NATS feature is disabled
#[cfg(not(feature = "nats"))]
async fn sse_stream(_: State<AppState>, _: AuthContext) -> Result<axum::response::Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>, StatusCode> {
    Err(StatusCode::SERVICE_UNAVAILABLE)
}

// Hygiene management endpoints
async fn get_hygiene_stats(State(state): State<AppState>, auth: AuthContext) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Only curators can view hygiene stats
    if !auth.roles.iter().any(|r| r == "curator") { 
        return Err((StatusCode::FORBIDDEN, "curator role required".into())); 
    }
    
    let stats = match state.hygiene_stats.lock() {
        Ok(stats) => stats.clone(),
        Err(_) => hygiene::HygieneStats::default(),
    };
    
    Ok(Json(json!({
        "runs_completed": stats.runs_completed,
        "total_breadcrumbs_purged": stats.total_breadcrumbs_purged,
        "total_agents_cleaned": stats.total_agents_cleaned,
        "last_run_duration_ms": stats.last_run_duration_ms,
        "last_run_errors": stats.last_run_errors,
        "hygiene_enabled": true,
        "last_updated": chrono::Utc::now().to_rfc3339()
    })))
}

async fn trigger_hygiene_run(State(state): State<AppState>, auth: AuthContext) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Only curators can trigger manual hygiene runs
    if !auth.roles.iter().any(|r| r == "curator") {
        return Err((StatusCode::FORBIDDEN, "curator role required".into()));
    }
    
    tracing::info!("Manual hygiene run triggered by agent: {}", auth.agent_id);
    
    // Use direct cleanup functions for immediate results
    let health_checks_purged = hygiene::cleanup_health_checks(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
    let expired_purged = hygiene::cleanup_expired_breadcrumbs(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
    let total_cleaned = health_checks_purged + expired_purged;
    
    tracing::info!("Manual hygiene completed: {} breadcrumbs cleaned", total_cleaned);
    
    Ok(Json(json!({
        "triggered": true,
        "health_checks_purged": health_checks_purged,
        "expired_breadcrumbs_purged": expired_purged,
        "total_cleaned": total_cleaned,
        "message": "Manual hygiene run completed successfully"
    })))
}

// ============ POINTER TAG HELPERS ============
// Support for hybrid pointer system: tags ARE pointers

fn is_state_tag(tag: &str) -> bool {
    matches!(tag,
        "approved" | "validated" | "bootstrap" | "deprecated" |
        "draft" | "archived" | "ephemeral" | "error" | "warning" | "info"
    )
}

fn is_domain_term(word: &str) -> bool {
    // RCRT domain vocabulary
    // Sync with context-builder entity_extractor.rs
    matches!(word,
        "breadcrumb" | "breadcrumbs" | "agent" | "agents" | "tool" | "tools" |
        "context" | "embedding" | "semantic" | "vector" | "schema" | "secret" |
        "create" | "search" | "execute" | "configure" | "update" | "delete" |
        "deno" | "typescript" | "rust" | "postgresql" | "browser" | "playwright" |
        "permission" | "workflow" | "catalog" | "validation" | "security" |
        "automation" | "testing" | "scraping" | "analysis" | "pattern" |
        "knowledge" | "guide" | "documentation" | "integration" | "calling" |
        "openai" | "anthropic" | "claude" | "openrouter" | "venice" |
        "database" | "postgres" | "crud" | "operations" | "query"
    )
}

fn extract_keywords_simple(text: &str) -> Vec<String> {
    let text_lower = text.to_lowercase();
    
    // Simple word extraction without regex (regex crate not in dependencies)
    // Split on whitespace and punctuation, keep domain terms
    let keywords: Vec<String> = text_lower
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| w.len() >= 4)  // 4+ chars
        .filter(|w| is_domain_term(w))
        .map(|s| s.to_string())
        .collect();
    
    keywords
}



