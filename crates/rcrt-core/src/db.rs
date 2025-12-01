use anyhow::Result;
use sqlx::{Pool, Postgres, postgres::PgPoolOptions, postgres::PgConnection};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use crate::models::{Breadcrumb, BreadcrumbCreate, BreadcrumbUpdate, Visibility, Sensitivity, BreadcrumbContextView, BreadcrumbFull, Selector, SelectorSubscription};
use sha2::{Digest, Sha256};
use pgvector::Vector;

#[derive(Clone)]
pub struct Db {
    pub pool: Pool<Postgres>,
}

impl Db {
    pub async fn connect(database_url: &str, current_owner_id: Uuid, current_agent_id: Option<Uuid>) -> Result<Self> {
        let owner = current_owner_id;
        let agent = current_agent_id;
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .after_connect(move |conn, _meta| {
                let owner = owner;
                let agent = agent;
                Box::pin(async move {
                    sqlx::query("select set_config('app.current_owner_id', $1, false)")
                        .bind(owner.to_string())
                        .execute(&mut *conn)
                        .await?;
                    if let Some(agent_id) = agent {
                        sqlx::query("select set_config('app.current_agent_id', $1, false)")
                            .bind(agent_id.to_string())
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(database_url)
            .await?;

        Ok(Self { pool })
    }

    pub async fn create_breadcrumb_for(&self, owner_id: Uuid, agent_id: Option<Uuid>, created_by: Option<Uuid>, req: BreadcrumbCreate) -> Result<Breadcrumb> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        self.create_breadcrumb_conn(&mut conn, owner_id, created_by, req, None).await
    }

    pub async fn create_breadcrumb_with_embedding_for(&self, owner_id: Uuid, agent_id: Option<Uuid>, created_by: Option<Uuid>, req: BreadcrumbCreate, embedding: Option<Vec<f32>>) -> Result<Breadcrumb> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        self.create_breadcrumb_conn(&mut conn, owner_id, created_by, req, embedding).await
    }

    async fn create_breadcrumb_conn(&self, conn: &mut PgConnection, owner_id: Uuid, created_by: Option<Uuid>, req: BreadcrumbCreate, embedding: Option<Vec<f32>>) -> Result<Breadcrumb> {
        let checksum = checksum_json(&req.context);
        let size_bytes = serde_json::to_vec(&req.context)?.len() as i32;
        let visibility = req.visibility.unwrap_or(Visibility::Team);
        let sensitivity = req.sensitivity.unwrap_or(Sensitivity::Low);

        let rec = sqlx::query_as::<_, DbBreadcrumb>(
            r#"insert into breadcrumbs 
            (owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility, sensitivity, version, checksum, ttl, created_by, updated_by, size_bytes, created_at, updated_at, embedding, entity_keywords)
            values
            ($1, $2, $3, $4, $5, $6, $7, $8, $9::visibility, $10::sensitivity, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
            returning id, owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility::text as visibility, sensitivity::text as sensitivity, version, checksum, ttl, created_at, updated_at, created_by, updated_by, size_bytes, embedding, entity_keywords"#,
        )
        .bind(owner_id)
        .bind(req.title)
        .bind(req.description)
        .bind(req.semantic_version)
        .bind(req.context)
        .bind(&req.tags[..])
        .bind(req.schema_name)
        .bind(req.llm_hints)
        .bind(visibility_to_db(&visibility))
        .bind(sensitivity_to_db(&sensitivity))
        .bind(1) // version
        .bind(checksum)
        .bind(req.ttl)
        .bind(created_by)
        .bind(created_by) // updated_by same as created_by on create
        .bind(size_bytes)
        .bind(Utc::now()) // created_at
        .bind(Utc::now()) // updated_at
        .bind(embedding.map(Vector::from))
        .bind(req.entity_keywords.as_deref())
        .fetch_one(&mut *conn)
        .await?;

        // write history v1
        sqlx::query(
            r#"insert into breadcrumb_history (breadcrumb_id, version, context, updated_at, updated_by, checksum)
               values ($1, $2, $3, now(), $4, $5) on conflict do nothing"#
        )
        .bind(rec.id)
        .bind(rec.version)
        .bind(&rec.context)
        .bind(rec.created_by)
        .bind(&rec.checksum)
        .execute(&mut *conn)
        .await?;

        Ok(rec.into())
    }

    pub async fn get_breadcrumb_context_for(&self, owner_id: Uuid, agent_id: Option<Uuid>, id: Uuid) -> Result<Option<BreadcrumbContextView>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        self.get_breadcrumb_context_conn(&mut conn, id).await
    }

    async fn get_breadcrumb_context_conn(&self, conn: &mut PgConnection, id: Uuid) -> Result<Option<BreadcrumbContextView>> {
        let rec = sqlx::query_as::<_, DbBreadcrumb>(
            r#"select id, owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility::text as visibility, sensitivity::text as sensitivity, version, checksum, ttl, created_at, updated_at, created_by, updated_by, size_bytes, embedding, entity_keywords
            from breadcrumbs where id = $1"#,
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;

        Ok(rec.map(|r| BreadcrumbContextView {
            id: r.id,
            title: r.title,
            description: r.description,
            semantic_version: r.semantic_version,
            context: r.context,
            tags: r.tags,
            schema_name: r.schema_name,
            llm_hints: r.llm_hints,
            version: r.version,
            updated_at: r.updated_at,
        }))
    }

    pub async fn get_breadcrumb_full_for(&self, owner_id: Uuid, agent_id: Option<Uuid>, id: Uuid) -> Result<Option<BreadcrumbFull>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        let rec = sqlx::query_as::<_, DbBreadcrumb>(
            r#"select id, owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility::text as visibility, sensitivity::text as sensitivity, version, checksum, ttl, created_at, updated_at, created_by, updated_by, size_bytes, embedding, entity_keywords
            from breadcrumbs where id = $1"#,
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(rec.map(|r| BreadcrumbFull {
            id: r.id, owner_id: r.owner_id, title: r.title, description: r.description, semantic_version: r.semantic_version,
            context: r.context, tags: r.tags, schema_name: r.schema_name, llm_hints: r.llm_hints,
            visibility: match r.visibility.as_str() {"public"=>Visibility::Public, "team"=>Visibility::Team, _=>Visibility::Private},
            sensitivity: match r.sensitivity.as_str() {"pii"=>Sensitivity::Pii, "secret"=>Sensitivity::Secret, _=>Sensitivity::Low},
            version: r.version, checksum: r.checksum, ttl: r.ttl, created_at: r.created_at, updated_at: r.updated_at,
            created_by: r.created_by, updated_by: r.updated_by, size_bytes: r.size_bytes, embedding: r.embedding, entity_keywords: r.entity_keywords
        }))
    }

    pub async fn create_selector_subscription(&self, owner_id: Uuid, agent_id: Uuid, selector: Selector) -> Result<SelectorSubscription> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let rec = sqlx::query_as::<_, DbSelector>(
            r#"insert into selector_subscriptions (owner_id, agent_id, selector)
            values ($1,$2,$3) returning id, owner_id, agent_id, selector"#,
        )
        .bind(owner_id)
        .bind(agent_id)
        .bind(serde_json::to_value(&selector)?)
        .fetch_one(&mut *conn)
        .await?;
        Ok(SelectorSubscription { id: rec.id, owner_id: rec.owner_id, agent_id: rec.agent_id, selector })
    }

    pub async fn list_selector_subscriptions(&self, owner_id: Uuid, agent_id: Uuid) -> Result<Vec<SelectorSubscription>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let rows = sqlx::query_as::<_, DbSelector>(
                r#"select id, owner_id, agent_id, selector from selector_subscriptions where owner_id = $1 and agent_id = $2"#,
            )
            .bind(owner_id)
            .bind(agent_id)
            .fetch_all(&mut *conn)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows { out.push(SelectorSubscription { id: r.id, owner_id: r.owner_id, agent_id: r.agent_id, selector: serde_json::from_value(r.selector)? }); }
        Ok(out)
    }

    pub async fn list_selector_subscriptions_for_owner(&self, owner_id: Uuid) -> Result<Vec<SelectorSubscription>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let rows = sqlx::query_as::<_, DbSelector>(
                r#"select id, owner_id, agent_id, selector from selector_subscriptions where owner_id = $1"#,
            )
            .bind(owner_id)
            .fetch_all(&mut *conn)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows { out.push(SelectorSubscription { id: r.id, owner_id: r.owner_id, agent_id: r.agent_id, selector: serde_json::from_value(r.selector)? }); }
        Ok(out)
    }

    pub async fn create_agent_webhook(&self, owner_id: Uuid, agent_id: Uuid, url: &str) -> Result<Uuid> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"insert into agent_webhooks (agent_id, url) values ($1,$2)
                on conflict (agent_id, url) do update set active = true
                returning id"#
        )
        .bind(agent_id)
        .bind(url)
        .fetch_one(&mut *conn)
        .await?;
        Ok(id)
    }

    pub async fn list_agent_webhooks(&self, owner_id: Uuid, agent_id: Uuid) -> Result<Vec<(Uuid, String)>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let rows = sqlx::query_as::<_, (Uuid, String)>(
            r#"select id, url from agent_webhooks where agent_id = $1 and active = true"#
        )
        .bind(agent_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows)
    }

    pub async fn deactivate_agent_webhook(&self, owner_id: Uuid, agent_id: Uuid, webhook_id: Uuid) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let res = sqlx::query(
            r#"update agent_webhooks set active = false where id = $1 and agent_id = $2"#
        )
        .bind(webhook_id)
        .bind(agent_id)
        .execute(&mut *conn)
        .await?;
        Ok(res.rows_affected() as i64)
    }

    pub async fn get_agent_webhook_secret(&self, owner_id: Uuid, agent_id: Uuid) -> Result<Option<String>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let row = sqlx::query_scalar::<_, Option<String>>(
            r#"select webhook_secret from agents where id = $1"#
        )
        .bind(agent_id)
        .fetch_one(&mut *conn)
        .await?;
        Ok(row)
    }

    pub async fn upsert_agent(&self, owner_id: Uuid, agent_id: Uuid, roles: Vec<String>) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        sqlx::query(
            r#"insert into agents (id, owner_id, agent_key, roles)
               values ($1,$2,$3,$4)
               on conflict (id) do update set roles = excluded.roles"#
        )
        .bind(agent_id)
        .bind(owner_id)
        .bind(agent_id.to_string())
        .bind(&roles[..])
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn set_agent_webhook_secret(&self, owner_id: Uuid, agent_id: Uuid, secret: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        sqlx::query(
            r#"update agents set webhook_secret = $2 where id = $1"#
        )
        .bind(agent_id)
        .bind(secret)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    // Secrets: create (expects caller to supply enc_blob and dek_encrypted)
    pub async fn create_secret(&self, owner_id: Uuid, name: &str, scope_type: &str, scope_id: Option<Uuid>, enc_blob: &[u8], dek_encrypted: &[u8], kek_id: &str) -> Result<Uuid> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let id = sqlx::query_scalar::<_, Uuid>(r#"insert into secrets (owner_id, name, scope_type, scope_id, enc_blob, dek_encrypted, kek_id) values ($1,$2,$3,$4,$5,$6,$7) returning id"#)
            .bind(owner_id)
            .bind(name)
            .bind(scope_type)
            .bind(scope_id)
            .bind(enc_blob)
            .bind(dek_encrypted)
            .bind(kek_id)
            .fetch_one(&mut *conn)
            .await?;
        // audit
        sqlx::query(r#"insert into secret_audit (secret_id, action) values ($1,'create')"#)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        Ok(id)
    }

    pub async fn get_secret_material(&self, owner_id: Uuid, secret_id: Uuid) -> Result<Option<(Vec<u8>, Vec<u8>, String)>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let row = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, String)>(r#"select enc_blob, dek_encrypted, kek_id from secrets where id = $1"#)
            .bind(secret_id)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row)
    }

    pub async fn audit_secret(&self, secret_id: Uuid, agent_id: Option<Uuid>, action: &str, reason: Option<&str>) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(r#"insert into secret_audit (secret_id, agent_id, action, reason) values ($1,$2,$3,$4)"#)
            .bind(secret_id)
            .bind(agent_id)
            .bind(action)
            .bind(reason)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    pub async fn set_breadcrumb_embedding(&self, owner_id: Uuid, agent_id: Option<Uuid>, id: Uuid, embedding: Vec<f32>) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        sqlx::query(
            r#"update breadcrumbs set embedding = $2 where id = $1"#
        )
        .bind(id)
        .bind(Vector::from(embedding))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn enqueue_webhook_dlq(&self, owner_id: Uuid, agent_id: Uuid, url: &str, payload: &serde_json::Value, last_error: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        sqlx::query(
            r#"insert into webhook_dlq (owner_id, agent_id, url, payload, last_error) values ($1,$2,$3,$4,$5)"#
        )
        .bind(owner_id)
        .bind(agent_id)
        .bind(url)
        .bind(payload)
        .bind(last_error)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn purge_expired_for_owner(&self, owner_id: Uuid) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let res = sqlx::query(r#"delete from breadcrumbs where owner_id = $1 and ttl is not null and ttl < now()"#)
            .bind(owner_id)
            .execute(&mut *conn)
            .await?;
        Ok(res.rows_affected() as i64)
    }

    pub async fn list_webhook_dlq(&self, owner_id: Uuid) -> Result<Vec<(Uuid, Uuid, String, serde_json::Value, Option<String>, DateTime<Utc>)>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, JsonValue, Option<String>, DateTime<Utc>)>(
            r#"select id, agent_id, url, payload, last_error, created_at from webhook_dlq where owner_id=$1 order by created_at desc"#
        )
        .bind(owner_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows)
    }

    pub async fn get_webhook_dlq(&self, owner_id: Uuid, id: Uuid) -> Result<Option<(Uuid, Uuid, String, serde_json::Value)>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let row = sqlx::query_as::<_, (Uuid, Uuid, String, JsonValue)>(
            r#"select id, agent_id, url, payload from webhook_dlq where id=$1 and owner_id=$2"#
        )
        .bind(id)
        .bind(owner_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row)
    }

    pub async fn delete_webhook_dlq(&self, owner_id: Uuid, id: Uuid) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, None).await?;
        let res = sqlx::query(r#"delete from webhook_dlq where id=$1 and owner_id=$2"#)
            .bind(id)
            .bind(owner_id)
            .execute(&mut *conn)
            .await?;
        Ok(res.rows_affected() as i64)
    }

    pub async fn record_idempotency(&self, owner_id: Uuid, agent_id: Option<Uuid>, key: &str, resource_type: &str, resource_id: Option<Uuid>) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        // Try insert; on conflict return false (already seen)
        let res = sqlx::query(
            r#"insert into idempotency_keys (key, owner_id, agent_id, resource_type, resource_id)
            values ($1,$2,$3,$4,$5) on conflict (key) do nothing"#
        )
        .bind(key)
        .bind(owner_id)
        .bind(agent_id)
        .bind(resource_type)
        .bind(resource_id)
        .execute(&mut *conn)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    pub async fn list_breadcrumb_history(&self, owner_id: Uuid, agent_id: Option<Uuid>, id: Uuid) -> Result<Vec<(i32, JsonValue, DateTime<Utc>, Option<Uuid>)>> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, agent_id).await?;
        let rows = sqlx::query_as::<_, (i32, JsonValue, DateTime<Utc>, Option<Uuid>)>(
            r#"select version, context, updated_at, updated_by from breadcrumb_history where breadcrumb_id = $1 order by version desc"#
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows)
    }

    pub async fn update_breadcrumb(&self, owner_id: Uuid, agent_id: Uuid, id: Uuid, expected_version: Option<i32>, u: BreadcrumbUpdate) -> Result<Breadcrumb> {
        tracing::info!("🔧 DB: update_breadcrumb called for {} by agent {}", id, agent_id);
        tracing::info!("🔧 DB: Update contains - title: {:?}, context: {}, tags: {:?}", 
            u.title, u.context.is_some(), u.tags);
        
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        // Fetch current
        let cur = sqlx::query_as::<_, DbBreadcrumb>(
            r#"select id, owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility::text as visibility, sensitivity::text as sensitivity, version, checksum, ttl, created_at, updated_at, created_by, updated_by, size_bytes, embedding, entity_keywords from breadcrumbs where id = $1"#
        )
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        
        tracing::info!("🔧 DB: Current breadcrumb version={}, context_preview={}", 
            cur.version, 
            serde_json::to_string(&cur.context).unwrap_or_default().chars().take(100).collect::<String>()
        );
        
        if let Some(ev) = expected_version { 
            if cur.version != ev { 
                tracing::warn!("🔧 DB: Version mismatch! Expected: {}, Current: {}", ev, cur.version);
                anyhow::bail!("version_mismatch"); 
            } 
        }

        let new_title = u.title.unwrap_or(cur.title);
        let new_description = u.description.or(cur.description);              // NEW
        let new_semantic_version = u.semantic_version.or(cur.semantic_version); // NEW
        let new_context = u.context.unwrap_or(cur.context);
        let new_tags = u.tags.unwrap_or(cur.tags);
        let new_schema = u.schema_name.or(cur.schema_name);
        let new_llm_hints = u.llm_hints.or(cur.llm_hints);                    // NEW
        let new_visibility = u.visibility.map(|v| visibility_to_db(&v)).unwrap_or(cur.visibility.as_str());
        let new_sensitivity = u.sensitivity.map(|s| sensitivity_to_db(&s)).unwrap_or(cur.sensitivity.as_str());
        let new_ttl = u.ttl.or(cur.ttl);
        let new_checksum = checksum_json(&new_context);
        let new_size = serde_json::to_vec(&new_context)?.len() as i32;
        let new_version = cur.version + 1;
        
        tracing::info!("🔧 DB: New values - version: {} -> {}, context_size: {} bytes, context_preview: {}", 
            cur.version, new_version, new_size,
            serde_json::to_string(&new_context).unwrap_or_default().chars().take(100).collect::<String>()
        );

        // entity_keywords preserved on update (not modified via PATCH)
        let rec = sqlx::query_as::<_, DbBreadcrumb>(
            r#"update breadcrumbs set title=$2, description=$3, semantic_version=$4, context=$5, tags=$6, schema_name=$7, llm_hints=$8,
                 visibility=$9::visibility, sensitivity=$10::sensitivity, version=$11, checksum=$12,
                 ttl=$13, updated_at=now(), updated_by=$14, size_bytes=$15
               where id=$1 returning id, owner_id, title, description, semantic_version, context, tags, schema_name, llm_hints, visibility::text as visibility, sensitivity::text as sensitivity, version, checksum, ttl, created_at, updated_at, created_by, updated_by, size_bytes, embedding, entity_keywords"#
        )
        .bind(id)
        .bind(&new_title)
        .bind(&new_description)             // NEW
        .bind(&new_semantic_version)        // NEW
        .bind(&new_context)
        .bind(&new_tags)
        .bind(&new_schema)
        .bind(&new_llm_hints)               // NEW
        .bind(new_visibility)
        .bind(new_sensitivity)
        .bind(new_version)
        .bind(&new_checksum)
        .bind(new_ttl)
        .bind(agent_id)
        .bind(new_size)
        .fetch_one(&mut *conn)
        .await?;
        
        tracing::info!("🔧 DB: SQL UPDATE completed successfully! Returned version={}, context_preview={}", 
            rec.version,
            serde_json::to_string(&rec.context).unwrap_or_default().chars().take(100).collect::<String>()
        );

        // Append history
        sqlx::query(r#"insert into breadcrumb_history (breadcrumb_id, version, context, updated_at, updated_by, checksum) values ($1,$2,$3, now(), $4, $5)"#)
            .bind(id)
            .bind(new_version)
            .bind(&new_context)
            .bind(agent_id)
            .bind(&new_checksum)
            .execute(&mut *conn)
            .await?;

        Ok(rec.into())
    }

    pub async fn delete_breadcrumb(&self, owner_id: Uuid, agent_id: Uuid, id: Uuid) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let res = sqlx::query(r#"delete from breadcrumbs where id = $1"#)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        Ok(res.rows_affected() as i64)
    }
}

impl Db {
    pub async fn list_secrets(&self, owner_id: Uuid, scope_type: Option<&str>, scope_id: Option<Uuid>) -> Result<Vec<(Uuid, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>)>> {
        let mut query = String::from("select id, name, scope_type, scope_id, created_at from secrets where owner_id = $1");
        if scope_type.is_some() { query.push_str(" and scope_type = $2"); }
        if scope_id.is_some() { query.push_str(" and scope_id = $3"); }
        query.push_str(" order by created_at desc");
        
        let rows = if let Some(st) = scope_type {
            if let Some(sid) = scope_id {
                sqlx::query_as::<_, (Uuid, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>)>(&query)
                    .bind(owner_id)
                    .bind(st)
                    .bind(sid)
                    .fetch_all(&self.pool)
                    .await?
            } else {
                sqlx::query_as::<_, (Uuid, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>)>(&query)
                    .bind(owner_id)
                    .bind(st)
                    .fetch_all(&self.pool)
                    .await?
            }
        } else {
            sqlx::query_as::<_, (Uuid, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>)>(&query)
                .bind(owner_id)
                .fetch_all(&self.pool)
                .await?
        };
        Ok(rows)
    }

    pub async fn update_secret(&self, owner_id: Uuid, secret_id: Uuid, enc_blob: &[u8], dek_encrypted: &[u8]) -> Result<()> {
        let rows = sqlx::query(
            "update secrets set enc_blob = $1, dek_encrypted = $2, updated_at = now() where id = $3 and owner_id = $4"
        )
        .bind(enc_blob)
        .bind(dek_encrypted)
        .bind(secret_id)
        .bind(owner_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        
        if rows == 0 {
            anyhow::bail!("secret not found or not owned");
        }
        Ok(())
    }

    pub async fn delete_secret(&self, owner_id: Uuid, secret_id: Uuid) -> Result<u64> {
        let result = sqlx::query(
            "delete from secrets where id = $1 and owner_id = $2"
        )
        .bind(secret_id)
        .bind(owner_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // Agent CRUD operations
    pub async fn list_agents(&self, owner_id: Uuid) -> Result<Vec<(Uuid, Vec<String>, chrono::DateTime<chrono::Utc>)>> {
        let rows = sqlx::query_as::<_, (Uuid, Vec<String>, chrono::DateTime<chrono::Utc>)>(
            "select id, roles, created_at from agents where owner_id = $1 order by created_at desc"
        )
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
    
    pub async fn get_agent(&self, owner_id: Uuid, agent_id: Uuid) -> Result<Option<(Uuid, Vec<String>, chrono::DateTime<chrono::Utc>)>> {
        let row = sqlx::query_as::<_, (Uuid, Vec<String>, chrono::DateTime<chrono::Utc>)>(
            "select id, roles, created_at from agents where owner_id = $1 and id = $2"
        )
        .bind(owner_id)
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
    
    pub async fn delete_agent(&self, owner_id: Uuid, agent_id: Uuid) -> Result<()> {
        sqlx::query("delete from agents where owner_id = $1 and id = $2")
            .bind(owner_id)
            .bind(agent_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    
    // Tenant CRUD operations
    pub async fn list_tenants(&self) -> Result<Vec<(Uuid, String, chrono::DateTime<chrono::Utc>)>> {
        let rows = sqlx::query_as::<_, (Uuid, String, chrono::DateTime<chrono::Utc>)>(
            "select id, name, created_at from tenants order by created_at desc"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
    
    pub async fn get_tenant(&self, tenant_id: Uuid) -> Result<Option<(Uuid, String, chrono::DateTime<chrono::Utc>)>> {
        let row = sqlx::query_as::<_, (Uuid, String, chrono::DateTime<chrono::Utc>)>(
            "select id, name, created_at from tenants where id = $1"
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
    
    pub async fn update_tenant(&self, tenant_id: Uuid, name: &str) -> Result<()> {
        sqlx::query("update tenants set name = $2 where id = $1")
            .bind(tenant_id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    
    pub async fn delete_tenant(&self, tenant_id: Uuid) -> Result<()> {
        sqlx::query("delete from tenants where id = $1")
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    
    // Selector CRUD operations
    pub async fn update_selector(&self, owner_id: Uuid, agent_id: Uuid, selector_id: Uuid, selector: Selector) -> Result<()> {
        sqlx::query(
            "update selectors set selector = $4 where id = $1 and owner_id = $2 and agent_id = $3"
        )
        .bind(selector_id)
        .bind(owner_id)
        .bind(agent_id)
        .bind(JsonValue::from(serde_json::to_value(selector)?))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    
    pub async fn delete_selector(&self, owner_id: Uuid, agent_id: Uuid, selector_id: Uuid) -> Result<()> {
        sqlx::query("delete from selectors where id = $1 and owner_id = $2 and agent_id = $3")
            .bind(selector_id)
            .bind(owner_id)
            .bind(agent_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    
    // ACL operations
    pub async fn list_acls(&self, owner_id: Uuid) -> Result<Vec<(Uuid, Uuid, Option<Uuid>, Vec<String>, chrono::DateTime<chrono::Utc>)>> {
        let rows = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>, Vec<String>, chrono::DateTime<chrono::Utc>)>(
            "select id, breadcrumb_id, grantee_agent_id, actions, created_at from acl_entries where owner_id = $1 order by created_at desc"
        )
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn has_acl_action(&self, owner_id: Uuid, agent_id: Uuid, breadcrumb_id: Uuid, action: &str) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(agent_id)).await?;
        let row = sqlx::query_scalar::<_, i64>(
            r#"select count(*) from acl_entries a where a.breadcrumb_id = $1 and (
                a.grantee_agent_id = $2 or a.grantee_owner_id = $3
            ) and $4 = any(a.actions)"#
        )
        .bind(breadcrumb_id)
        .bind(agent_id)
        .bind(owner_id)
        .bind(action)
        .fetch_one(&mut *conn)
        .await?;
        Ok(row > 0)
    }

    pub async fn grant_acl_agent(&self, owner_id: Uuid, breadcrumb_id: Uuid, grantee_agent_id: Uuid, action: &str) -> Result<Uuid> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(grantee_agent_id)).await?;
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"insert into acl_entries (owner_id, breadcrumb_id, grantee_agent_id, actions)
               values ($1,$2,$3, ARRAY[$4]::acl_action[]) returning id"#
        )
        .bind(owner_id)
        .bind(breadcrumb_id)
        .bind(grantee_agent_id)
        .bind(action)
        .fetch_one(&mut *conn)
        .await?;
        Ok(id)
    }

    pub async fn revoke_acl_agent(&self, owner_id: Uuid, breadcrumb_id: Uuid, grantee_agent_id: Uuid, action: &str) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        set_rls(&mut conn, owner_id, Some(grantee_agent_id)).await?;
        let rows = sqlx::query(
            r#"delete from acl_entries where owner_id=$1 and breadcrumb_id=$2 and grantee_agent_id=$3 and $4 = any(actions)"#
        )
        .bind(owner_id)
        .bind(breadcrumb_id)
        .bind(grantee_agent_id)
        .bind(action)
        .execute(&mut *conn)
        .await?
        .rows_affected() as i64;
        Ok(rows)
    }

    pub async fn ensure_tenant(&self, tenant_id: Uuid, name: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(
            r#"insert into tenants (id, name) values ($1,$2) on conflict (id) do update set name = excluded.name"#
        )
        .bind(tenant_id)
        .bind(name)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

fn visibility_to_db(v: &Visibility) -> &'static str {
    match v { Visibility::Public => "public", Visibility::Team => "team", Visibility::Private => "private" }
}

fn sensitivity_to_db(s: &Sensitivity) -> &'static str {
    match s { Sensitivity::Low => "low", Sensitivity::Pii => "pii", Sensitivity::Secret => "secret" }
}

fn checksum_json(v: &JsonValue) -> String {
    let mut hasher = Sha256::new();
    let bytes = serde_json::to_vec(v).expect("json to bytes");
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

async fn set_rls(conn: &mut PgConnection, owner_id: Uuid, agent_id: Option<Uuid>) -> Result<()> {
    sqlx::query("select set_config('app.current_owner_id', $1, false)")
        .bind(owner_id.to_string())
        .execute(&mut *conn)
        .await?;
    if let Some(agent) = agent_id {
        sqlx::query("select set_config('app.current_agent_id', $1, false)")
            .bind(agent.to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct DbBreadcrumb {
    id: Uuid,
    owner_id: Uuid,
    title: String,
    description: Option<String>,        // NEW
    semantic_version: Option<String>,   // NEW
    context: JsonValue,
    tags: Vec<String>,
    schema_name: Option<String>,
    llm_hints: Option<JsonValue>,       // NEW
    #[sqlx(rename = "visibility")]
    visibility: String,
    #[sqlx(rename = "sensitivity")]
    sensitivity: String,
    version: i32,
    checksum: String,
    ttl: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    updated_by: Option<Uuid>,
    size_bytes: i32,
    embedding: Option<Vector>,
    entity_keywords: Option<Vec<String>>,  // Hybrid pointers
}

impl From<DbBreadcrumb> for Breadcrumb {
    fn from(r: DbBreadcrumb) -> Self {
        Breadcrumb {
            id: r.id,
            owner_id: r.owner_id,
            title: r.title,
            description: r.description,             // NEW
            semantic_version: r.semantic_version,   // NEW
            context: r.context,
            tags: r.tags,
            schema_name: r.schema_name,
            llm_hints: r.llm_hints,                 // NEW
            visibility: match r.visibility.as_str() {"public"=>Visibility::Public, "team"=>Visibility::Team, _=>Visibility::Private},
            sensitivity: match r.sensitivity.as_str() {"pii"=>Sensitivity::Pii, "secret"=>Sensitivity::Secret, _=>Sensitivity::Low},
            version: r.version,
            checksum: r.checksum,
            ttl: r.ttl,
            created_at: r.created_at,
            updated_at: r.updated_at,
            created_by: r.created_by,
            updated_by: r.updated_by,
            size_bytes: r.size_bytes,
            entity_keywords: r.entity_keywords,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DbSelector { id: Uuid, owner_id: Uuid, agent_id: Uuid, selector: JsonValue }


