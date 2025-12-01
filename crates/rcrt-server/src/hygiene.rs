/**
 * RCRT Hygiene Runner
 * Automatic cleanup of expired breadcrumbs, agents, and system resources
 */

use std::time::Duration;
use uuid::Uuid;
use tokio::time::{interval, Instant};
use tracing::{info, warn, error};
use serde_json::json;
use crate::AppState;

// Helper function for error handling
fn internal_error<E: std::fmt::Display>(e: E) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

/// Simple direct cleanup function that can be called from API endpoints
pub async fn cleanup_health_checks(db: &rcrt_core::db::Db) -> Result<u64, sqlx::Error> {
    info!("Running direct health check cleanup...");
    
    // Clean up health check breadcrumbs older than 5 minutes (tag-based)
    let query = "DELETE FROM breadcrumbs 
                 WHERE tags @> ARRAY['health:check'] 
                 AND created_at < NOW() - INTERVAL '5 minutes'";
    
    let result = sqlx::query(query)
        .execute(&db.pool)
        .await?;
    
    let deleted = result.rows_affected();
    
    if deleted > 0 {
        info!("Cleaned up {} health check breadcrumbs", deleted);
    }
    
    Ok(deleted)
}

/// Simple cleanup for all expired breadcrumbs (tag-based, generic)
pub async fn cleanup_expired_breadcrumbs(db: &rcrt_core::db::Db) -> Result<u64, sqlx::Error> {
    info!("Running direct expired breadcrumb cleanup...");
    
    let mut total_deleted = 0u64;
    
    // 1. Explicit TTL breadcrumbs (most common case)
    let ttl_query = "DELETE FROM breadcrumbs WHERE ttl IS NOT NULL AND ttl < NOW()";
    let ttl_result = sqlx::query(ttl_query).execute(&db.pool).await?;
    total_deleted += ttl_result.rows_affected();
    
    // 2. Tag-based cleanup (no hardcoded schemas!)
    // Health checks (via tag)
    let health_query = "DELETE FROM breadcrumbs 
                       WHERE tags @> ARRAY['health:check']
                       AND created_at < NOW() - INTERVAL '5 minutes'
                       AND ttl IS NULL";
    let health_result = sqlx::query(health_query).execute(&db.pool).await?;
    total_deleted += health_result.rows_affected();
    
    // System pings (via tag)
    let ping_query = "DELETE FROM breadcrumbs 
                     WHERE tags @> ARRAY['system:ping']
                     AND created_at < NOW() - INTERVAL '10 minutes'
                     AND ttl IS NULL";
    let ping_result = sqlx::query(ping_query).execute(&db.pool).await?;
    total_deleted += ping_result.rows_affected();
    
    // Temporary data (via tag)
    let temp_query = "DELETE FROM breadcrumbs 
                     WHERE tags && ARRAY['temp:data', 'temp:state']
                     AND created_at < NOW() - INTERVAL '1 hour'
                     AND ttl IS NULL";
    let temp_result = sqlx::query(temp_query).execute(&db.pool).await?;
    total_deleted += temp_result.rows_affected();
    
    if total_deleted > 0 {
        info!("Cleaned up {} total expired breadcrumbs", total_deleted);
    }
    
    Ok(total_deleted)
}

#[derive(Debug, Clone)]
pub struct HygieneConfig {
    pub enabled: bool,
    pub run_interval_seconds: u64,
    pub batch_size: i64,
    pub max_delete_per_run: i64,
    
    // Breadcrumb expiry policies
    pub default_breadcrumb_ttl_hours: Option<i64>,
    pub healthcheck_ttl_minutes: i64,
    pub temp_data_ttl_hours: i64,
    pub log_retention_days: i64,
    
    // Agent expiry policies  
    pub agent_max_idle_hours: i64,
    pub agent_cleanup_on_exit: bool,
    
    // Performance tuning
    pub skip_during_peak_hours: bool,
    pub max_execution_time_seconds: u64,
}

impl Default for HygieneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            run_interval_seconds: 300, // 5 minutes
            batch_size: 100,
            max_delete_per_run: 1000,
            
            // Breadcrumb defaults
            default_breadcrumb_ttl_hours: None, // No default expiry
            healthcheck_ttl_minutes: 5,         // Health checks expire quickly
            temp_data_ttl_hours: 24,            // Temporary data lasts 1 day
            log_retention_days: 30,             // Logs kept for 30 days
            
            // Agent defaults
            agent_max_idle_hours: 48,           // Idle agents cleaned after 2 days
            agent_cleanup_on_exit: true,
            
            // Performance
            skip_during_peak_hours: false,
            max_execution_time_seconds: 60,     // 1 minute max per hygiene run
        }
    }
}

pub struct HygieneRunner {
    config: HygieneConfig,
    state: AppState,
}

#[derive(Debug, Clone, Default)]
pub struct HygieneStats {
    pub runs_completed: u64,
    pub total_breadcrumbs_purged: u64,
    pub total_agents_cleaned: u64,
    pub last_run_duration_ms: u64,
    pub last_run_errors: u32,
}

impl HygieneRunner {
    pub fn new(state: AppState, config: Option<HygieneConfig>) -> Self {
        let config = config.unwrap_or_default();
        
        info!(
            "Hygiene runner initialized - interval: {}s, healthcheck TTL: {}min", 
            config.run_interval_seconds,
            config.healthcheck_ttl_minutes
        );
        
        Self {
            config,
            state,
        }
    }
    
    /// Start the hygiene runner background task
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        if !self.config.enabled {
            info!("Hygiene runner disabled via configuration");
            return tokio::spawn(async {});
        }
        
        info!("Starting hygiene runner background task...");
        
        tokio::spawn(async move {
            info!("🧹 Hygiene background task spawned successfully");
            self.run_loop().await;
        })
    }
    
    async fn run_loop(self) {
        let mut interval = interval(Duration::from_secs(self.config.run_interval_seconds));
        
        info!("🧹 Hygiene runner loop starting - will run every {} seconds", self.config.run_interval_seconds);
        
        loop {
            interval.tick().await;
            info!("🧹 Hygiene cycle starting...");
            
            let run_start = Instant::now();
            
            match self.run_hygiene_cycle().await {
                Ok(()) => {
                    // Successful run
                }
                Err(e) => {
                    error!("Hygiene run failed: {}", e);
                    if let Ok(mut stats) = self.state.hygiene_stats.lock() {
                        stats.last_run_errors += 1;
                    }
                }
            }
            
            // Update shared stats
            let should_emit_stats = if let Ok(mut stats) = self.state.hygiene_stats.lock() {
                stats.runs_completed += 1;
                stats.last_run_duration_ms = run_start.elapsed().as_millis() as u64;
                
                // Check if we should emit stats (every 10 runs)
                stats.runs_completed % 10 == 0
            } else {
                false
            };
            
            // Emit hygiene stats periodically (outside mutex lock)
            if should_emit_stats {
                if let Err(e) = self.emit_hygiene_stats().await {
                    warn!("Failed to emit hygiene stats: {}", e);
                }
            }
        }
    }
    
    async fn run_hygiene_cycle(&self) -> Result<(), Box<dyn std::error::Error>> {
        let cycle_start = Instant::now();
        let current_run = if let Ok(stats) = self.state.hygiene_stats.lock() {
            stats.runs_completed + 1
        } else {
            0
        };
        info!("🧹 Starting hygiene cycle #{}", current_run);
        
        // Use the working direct cleanup functions
        let health_checks_cleaned = cleanup_health_checks(&self.state.db).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        
        let expired_cleaned = cleanup_expired_breadcrumbs(&self.state.db).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        
        let total_cleaned = health_checks_cleaned + expired_cleaned;
        
        // Update shared stats
        if let Ok(mut stats) = self.state.hygiene_stats.lock() {
            stats.total_breadcrumbs_purged += total_cleaned;
        }
        
        let duration = cycle_start.elapsed();
        
        if total_cleaned > 0 {
            info!("🧹 Hygiene cycle complete: {}ms, cleaned {} breadcrumbs", duration.as_millis(), total_cleaned);
        } else {
            info!("🧹 Hygiene cycle complete: {}ms, no cleanup needed", duration.as_millis());
        }
        
        Ok(())
    }
    
    async fn cleanup_expired_breadcrumbs(&self) -> Result<u64, Box<dyn std::error::Error>> {
        info!("Cleaning up explicitly expired breadcrumbs...");
        
        // 1. Clean up breadcrumbs with explicit datetime TTL that have expired
        let query = "DELETE FROM breadcrumbs WHERE ttl IS NOT NULL AND ttl < NOW()";
        let result = sqlx::query(query)
            .execute(&self.state.db.pool)
            .await?;
        
        let explicit_ttl_deleted = result.rows_affected();
        
        // 2. Apply tag-based TTL policies for breadcrumbs without explicit TTL
        let policy_deleted = self.apply_tag_ttl_policies().await?;
        
        let total = explicit_ttl_deleted + policy_deleted;
        
        if total > 0 {
            info!("Purged {} expired breadcrumbs (datetime: {}, tag-policy: {})", 
                total, explicit_ttl_deleted, policy_deleted);
        }
        
        Ok(total)
    }
    
    async fn apply_tag_ttl_policies(&self) -> Result<u64, Box<dyn std::error::Error>> {
        let mut total_deleted = 0u64;
        
        // Health check breadcrumbs (tag-based, no schema matching)
        let healthcheck_query = format!(
            "DELETE FROM breadcrumbs 
             WHERE tags @> ARRAY['health:check']
             AND created_at < NOW() - INTERVAL '{} minutes'
             AND ttl IS NULL",
            self.config.healthcheck_ttl_minutes
        );
        
        let result = sqlx::query(&healthcheck_query)
            .execute(&self.state.db.pool)
            .await?;
        
        total_deleted += result.rows_affected();
        
        // Temporary data (tag-based)
        let temp_data_query = format!(
            "DELETE FROM breadcrumbs 
             WHERE tags && ARRAY['temp:data', 'temp:state', 'temp:memory']
             AND created_at < NOW() - INTERVAL '{} hours'
             AND ttl IS NULL",
            self.config.temp_data_ttl_hours
        );
        
        let result = sqlx::query(&temp_data_query)
            .execute(&self.state.db.pool)
            .await?;
        
        total_deleted += result.rows_affected();
        
        // Execution logs (tag-based)
        let logs_query = format!(
            "DELETE FROM breadcrumbs 
             WHERE tags && ARRAY['log:execution', 'log:tool', 'log:error']
             AND created_at < NOW() - INTERVAL '{} days'
             AND ttl IS NULL",
            self.config.log_retention_days
        );
        
        let result = sqlx::query(&logs_query)
            .execute(&self.state.db.pool)
            .await?;
        
        total_deleted += result.rows_affected();
        
        // Performance metrics (tag-based)
        let metrics_query = "DELETE FROM breadcrumbs 
                            WHERE tags && ARRAY['metric:performance', 'metric:agent', 'metric:tool']
                            AND created_at < NOW() - INTERVAL '7 days'
                            AND ttl IS NULL";
        
        let result = sqlx::query(metrics_query)
            .execute(&self.state.db.pool)
            .await?;
        
        total_deleted += result.rows_affected();
        
        Ok(total_deleted)
    }
    
    async fn cleanup_expired_agents(&self) -> Result<u64, Box<dyn std::error::Error>> {
        info!("Cleaning up expired/idle agents...");
        
        // Find agents that haven't been active recently
        let idle_agents_query = format!(
            "SELECT DISTINCT a.id, a.owner_id 
             FROM agents a 
             LEFT JOIN breadcrumbs b ON a.id = b.created_by OR a.id = b.updated_by
             WHERE a.created_at < NOW() - INTERVAL '{} hours'
             AND (b.updated_at IS NULL OR b.updated_at < NOW() - INTERVAL '{} hours')",
            self.config.agent_max_idle_hours,
            self.config.agent_max_idle_hours
        );
        
        let idle_agents: Vec<(Uuid, Uuid)> = sqlx::query_as(&idle_agents_query)
            .fetch_all(&self.state.db.pool)
            .await?;
        
        let mut cleaned_count = 0u64;
        
        for (agent_id, owner_id) in idle_agents {
            // RCRT uses tag-based routing - no subscriptions table to check
            // Safe to cleanup idle agents directly
            match self.cleanup_agent(owner_id, agent_id).await {
                Ok(()) => {
                    cleaned_count += 1;
                    info!("Cleaned up idle agent: {}", agent_id);
                }
                Err(e) => {
                    warn!("Failed to cleanup agent {}: {}", agent_id, e);
                }
            }
        }
        
        Ok(cleaned_count)
    }
    
    async fn cleanup_agent(&self, owner_id: Uuid, agent_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Clean up agent's breadcrumbs (if configured)
        if self.config.agent_cleanup_on_exit {
            let agent_breadcrumbs_query = 
                "DELETE FROM breadcrumbs 
                 WHERE owner_id = $1 
                 AND (created_by = $2 OR updated_by = $2)
                 AND tags @> ARRAY['agent:memory', 'temp:data']";
            
            sqlx::query(agent_breadcrumbs_query)
                .bind(owner_id)
                .bind(agent_id)
                .execute(&self.state.db.pool)
                .await?;
        }
        
        // 2. Clean up agent's webhooks
        sqlx::query("UPDATE agent_webhooks SET active = false WHERE owner_id = $1 AND agent_id = $2")
            .bind(owner_id)
            .bind(agent_id)
            .execute(&self.state.db.pool)
            .await?;
        
        // 3. Delete the agent record
        sqlx::query("DELETE FROM agents WHERE owner_id = $1 AND id = $2")
            .bind(owner_id)
            .bind(agent_id)
            .execute(&self.state.db.pool)
            .await?;
        
        Ok(())
    }
    
    async fn cleanup_old_dlq_entries(&self) -> Result<u64, Box<dyn std::error::Error>> {
        // Clean up old webhook DLQ entries (older than 7 days)
        let old_dlq_query = 
            "DELETE FROM webhook_dlq 
             WHERE created_at < NOW() - INTERVAL '7 days'";
        
        let result = sqlx::query(old_dlq_query)
            .execute(&self.state.db.pool)
            .await?;
        
        let deleted = result.rows_affected();
        if deleted > 0 {
            info!("Cleaned up {} old DLQ entries", deleted);
        }
        
        Ok(deleted)
    }
    
    async fn apply_schema_ttl_policies(&self) -> Result<u64, Box<dyn std::error::Error>> {
        let policies = vec![
            // Health checks - very short lived
            (
                "health_checks",
                format!("schema_name = 'tool.request.v1' AND tags @> ARRAY['health:check'] AND created_at < NOW() - INTERVAL '{} minutes'", self.config.healthcheck_ttl_minutes)
            ),
            
            // Ping events - short lived
            (
                "ping_events", 
                "schema_name = 'system.ping.v1' AND created_at < NOW() - INTERVAL '10 minutes'".to_string()
            ),
            
            // Temporary tool responses - medium lived
            (
                "temp_tool_responses",
                "schema_name = 'tool.response.v1' AND tags @> ARRAY['temp:result'] AND created_at < NOW() - INTERVAL '1 hour'".to_string()
            ),
            
            // Agent thinking/analysis breadcrumbs - medium lived  
            (
                "agent_thinking",
                "schema_name IN ('agent.thinking.v1', 'agent.analysis.v1') AND created_at < NOW() - INTERVAL '6 hours'".to_string()
            ),
            
            // Old performance metrics - long lived but limited
            (
                "old_metrics",
                "schema_name LIKE '%.metrics.v1' AND created_at < NOW() - INTERVAL '30 days'".to_string()
            ),
        ];
        
        let mut total_deleted = 0u64;
        
        for (policy_name, condition) in policies {
            let query = format!(
                "DELETE FROM breadcrumbs 
                 WHERE {} 
                 AND ttl IS NULL 
                 LIMIT {}",
                condition,
                self.config.batch_size
            );
            
            match sqlx::query(&query).execute(&self.state.db.pool).await {
                Ok(result) => {
                    let deleted = result.rows_affected();
                    if deleted > 0 {
                        info!("Policy '{}' cleaned up {} breadcrumbs", policy_name, deleted);
                        total_deleted += deleted;
                    }
                }
                Err(e) => {
                    warn!("Policy '{}' failed: {}", policy_name, e);
                    // Error already handled above
                }
            }
        }
        
        Ok(total_deleted)
    }
    
    async fn emit_hygiene_stats(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Get current stats (avoid holding mutex across await)
        let current_stats = if let Ok(stats) = self.state.hygiene_stats.lock() {
            stats.clone()
        } else {
            return Ok(()); // Skip if can't get stats
        };
        
        // Create a hygiene stats breadcrumb for monitoring
        let stats_breadcrumb = json!({
            "title": "Hygiene Runner Statistics",
            "context": {
                "runs_completed": current_stats.runs_completed,
                "total_breadcrumbs_purged": current_stats.total_breadcrumbs_purged,
                "total_agents_cleaned": current_stats.total_agents_cleaned,
                "last_run_duration_ms": current_stats.last_run_duration_ms,
                "last_run_errors": current_stats.last_run_errors,
                "next_run_in_seconds": self.config.run_interval_seconds,
                "config": {
                    "healthcheck_ttl_minutes": self.config.healthcheck_ttl_minutes,
                    "temp_data_ttl_hours": self.config.temp_data_ttl_hours,
                    "agent_max_idle_hours": self.config.agent_max_idle_hours
                }
            },
            "tags": ["system:hygiene", "system:stats", "internal:monitoring"],
            "schema_name": "system.hygiene.v1",
            "ttl": chrono::Utc::now() + chrono::Duration::hours(1) // Stats expire after 1 hour
        });
        
        // Create via the database directly to avoid auth issues
        let owner_id = std::env::var("OWNER_ID")
            .ok()
            .and_then(|s| Uuid::parse_str(&s).ok())
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        
        let system_agent_id = std::env::var("AGENT_ID")
            .ok()
            .and_then(|s| Uuid::parse_str(&s).ok()) 
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-0000000000aa").unwrap());
        
        // Ensure the system agent exists (upsert will create if needed)
        if let Err(e) = self.state.db.upsert_agent(
            owner_id,
            system_agent_id,
            vec!["emitter".to_string(), "curator".to_string()]
        ).await {
            warn!("Unable to ensure system agent exists for hygiene stats: {}. Skipping stats emission.", e);
            return Ok(());
        }
        
        let _result = self.state.db.create_breadcrumb_with_embedding_for(
            owner_id,
            Some(system_agent_id),
            Some(system_agent_id),
            rcrt_core::models::BreadcrumbCreate {
                title: stats_breadcrumb["title"].as_str().unwrap().to_string(),
                description: Some("Hygiene runner statistics and status".to_string()),
                semantic_version: Some("1.0.0".to_string()),
                context: stats_breadcrumb["context"].clone(),
                tags: stats_breadcrumb["tags"].as_array().unwrap().iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect(),
                schema_name: Some(stats_breadcrumb["schema_name"].as_str().unwrap().to_string()),
                llm_hints: None,
                visibility: None,
                sensitivity: None,
                ttl: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                entity_keywords: None,
            },
            None
        ).await?;
        
        Ok(())
    }
}

/// Parse TTL duration from tag (e.g., "ttl:5min", "ttl:1hour", "ttl:7days")
fn parse_ttl_from_tags(tags: &[String]) -> Option<chrono::DateTime<chrono::Utc>> {
    for tag in tags {
        if let Some(duration_str) = tag.strip_prefix("ttl:") {
            if let Some(duration) = parse_duration(duration_str) {
                return Some(chrono::Utc::now() + duration);
            }
        }
    }
    None
}

/// Parse duration string like "5min", "1hour", "7days"
fn parse_duration(s: &str) -> Option<chrono::Duration> {
    // Extract number and unit
    let (num_str, unit) = if s.ends_with("min") {
        (s.trim_end_matches("min"), "min")
    } else if s.ends_with("hour") || s.ends_with("hours") {
        let s = s.trim_end_matches("hours").trim_end_matches("hour");
        (s, "hour")
    } else if s.ends_with("day") || s.ends_with("days") {
        let s = s.trim_end_matches("days").trim_end_matches("day");
        (s, "day")
    } else {
        return None;
    };
    
    let num: i64 = num_str.parse().ok()?;
    
    match unit {
        "min" => Some(chrono::Duration::minutes(num)),
        "hour" => Some(chrono::Duration::hours(num)),
        "day" => Some(chrono::Duration::days(num)),
        _ => None
    }
}

/// Enhanced TTL middleware for breadcrumb creation (tag-based, generic)
pub fn apply_auto_ttl(
    create_req: &mut rcrt_core::models::BreadcrumbCreate,
    schema_name: Option<&str>,
    tags: &[String]
) {
    // Don't override explicit TTL
    if create_req.ttl.is_some() {
        return;
    }
    
    // Try tag-based TTL first (e.g., "ttl:5min", "ttl:1hour")
    if let Some(ttl) = parse_ttl_from_tags(tags) {
        create_req.ttl = Some(ttl);
        return;
    }
    
    // Fallback: Common patterns via tags (not schemas!)
    // This enables gradual migration - eventually all will use ttl:* tags
    let auto_ttl = match () {
        _ if tags.iter().any(|t| t.contains("health:check")) => {
            Some(chrono::Utc::now() + chrono::Duration::minutes(5))
        },
        _ if tags.iter().any(|t| t == "system:ping") => {
            Some(chrono::Utc::now() + chrono::Duration::minutes(10))
        },
        _ if tags.iter().any(|t| t.starts_with("temp:")) => {
            Some(chrono::Utc::now() + chrono::Duration::hours(1))
        },
        _ if tags.iter().any(|t| t.contains("log:")) => {
            Some(chrono::Utc::now() + chrono::Duration::days(7))
        },
        _ => None
    };
    
    if let Some(ttl) = auto_ttl {
        create_req.ttl = Some(ttl);
    }
}

/// Configuration for hygiene runner from environment variables
pub fn load_hygiene_config() -> HygieneConfig {
    let enabled = std::env::var("HYGIENE_ENABLED")
        .unwrap_or_else(|_| "true".to_string())
        .parse()
        .unwrap_or(true);
    
    let interval = std::env::var("HYGIENE_INTERVAL_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300); // 5 minutes default
    
    info!("🧹 Loading hygiene config: enabled={}, interval={}s", enabled, interval);
    
    HygieneConfig {
        enabled,
        run_interval_seconds: interval,
        
        healthcheck_ttl_minutes: std::env::var("HYGIENE_HEALTHCHECK_TTL_MINUTES")
            .ok()
            .and_then(|s| s.parse().ok()) 
            .unwrap_or(5), // 5 minutes default
        
        temp_data_ttl_hours: std::env::var("HYGIENE_TEMP_DATA_TTL_HOURS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24), // 24 hours default
        
        agent_max_idle_hours: std::env::var("HYGIENE_AGENT_IDLE_HOURS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(48), // 48 hours default
        
        ..Default::default()
    }
}
