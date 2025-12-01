/*!
 * Context breadcrumb publisher
 */

use crate::{
    rcrt_client::RcrtClient,
    retrieval::AssembledContext,
    graph::BreadcrumbNode,
};
use anyhow::Result;
use std::sync::Arc;
use uuid::Uuid;

pub struct ContextPublisher {
    rcrt_client: Arc<RcrtClient>,
}

impl ContextPublisher {
    pub fn new(rcrt_client: Arc<RcrtClient>) -> Self {
        ContextPublisher { rcrt_client }
    }
    
    /// Extract LLM-optimized content from a supporting breadcrumb
    async fn extract_llm_content(&self, breadcrumb_id: Uuid) -> Result<serde_json::Value> {
        // Supporting breadcrumbs: use LLM-optimized endpoint
        let bc = self.rcrt_client.get_breadcrumb(breadcrumb_id).await
            .map_err(|e| anyhow::anyhow!("Failed to fetch LLM content for {}: {}", breadcrumb_id, e))?;
        Ok(bc.context)
    }
    
    /// Extract full breadcrumb structure (for trigger)
    async fn extract_full_breadcrumb(&self, breadcrumb_id: Uuid) -> Result<serde_json::Value> {
        // Trigger breadcrumb: use /full endpoint for complete metadata
        let bc = self.rcrt_client.get_breadcrumb_full(breadcrumb_id).await
            .map_err(|e| anyhow::anyhow!("Failed to fetch full breadcrumb for {}: {}", breadcrumb_id, e))?;
        
        // Format with all top-level metadata fields
        let mut full_structure = serde_json::json!({
            "id": bc.id,
            "title": bc.title,
            "tags": bc.tags,
            "schema_name": bc.schema_name,
            "context": bc.context,
        });
        
        // Include optional top-level fields
        if let Some(desc) = bc.description {
            full_structure["description"] = serde_json::Value::String(desc);
        }
        if let Some(ver) = bc.semantic_version {
            full_structure["semantic_version"] = serde_json::Value::String(ver);
        }
        if let Some(hints) = bc.llm_hints {
            full_structure["llm_hints"] = hints;
        }
        
        Ok(full_structure)
    }
    
    pub async fn publish_context(
        &self,
        consumer_id: &str,
        session_tag: &str,
        trigger_id: Option<Uuid>,
        context: &AssembledContext,
    ) -> Result<()> {
        let mut formatted_text = String::new();
        
        // ============ PHASE 1: TRIGGER BREADCRUMB (Full Structure) ============
        if let Some(trigger_uuid) = trigger_id {
            formatted_text.push_str("=== TRIGGER BREADCRUMB (Full Structure) ===\n\n");
            
            let full_trigger = self.extract_full_breadcrumb(trigger_uuid).await?;
            formatted_text.push_str(&self.format_content(full_trigger)?);
            formatted_text.push_str("\n\n");
        }
        
        // ============ PHASE 2: SUPPORTING CONTEXT (LLM-Optimized) ============
        
        // Group supporting breadcrumbs by schema (excluding trigger)
        let mut tools = Vec::new();
        let mut knowledge = Vec::new();
        let mut messages = Vec::new();
        let mut browser = Vec::new();
        let mut other = Vec::new();
        
        for bc in &context.breadcrumbs {
            // Skip trigger breadcrumb (already shown above)
            if Some(bc.id) == trigger_id {
                continue;
            }
            
            // Categorize by tags (generic, extensible)
            if bc.tags.iter().any(|t| t.starts_with("category:tool") || t.starts_with("tool:")) {
                tools.push(bc);
            } else if bc.tags.iter().any(|t| t.starts_with("category:knowledge") || t == "knowledge" || t == "note") {
                knowledge.push(bc);
            } else if bc.tags.iter().any(|t| t.starts_with("category:message") || t.starts_with("user:") || t.starts_with("agent:response")) {
                messages.push(bc);
            } else if bc.tags.iter().any(|t| t.starts_with("category:browser") || t.starts_with("browser:")) {
                browser.push(bc);
            } else {
                other.push(bc);
            }
        }
        
        // Format sections with LLM-optimized content
        if !tools.is_empty() {
            formatted_text.push_str("=== AVAILABLE TOOLS ===\n\n");
            for bc in tools {
                let llm_content = self.extract_llm_content(bc.id).await?;
                formatted_text.push_str(&self.format_content(llm_content)?);
                formatted_text.push_str("\n\n");
            }
        }
        
        if !browser.is_empty() {
            formatted_text.push_str("=== BROWSER CONTEXT ===\n\n");
            for bc in browser {
                let llm_content = self.extract_llm_content(bc.id).await?;
                formatted_text.push_str(&self.format_content(llm_content)?);
                formatted_text.push_str("\n\n");
            }
        }
        
        if !knowledge.is_empty() {
            formatted_text.push_str("=== RELEVANT KNOWLEDGE ===\n\n");
            for bc in knowledge {
                let llm_content = self.extract_llm_content(bc.id).await?;
                formatted_text.push_str(&self.format_content(llm_content)?);
                formatted_text.push_str("\n\n");
            }
        }
        
        if !messages.is_empty() {
            formatted_text.push_str("=== CONVERSATION HISTORY ===\n\n");
            for bc in messages {
                let llm_content = self.extract_llm_content(bc.id).await?;
                formatted_text.push_str(&self.format_content(llm_content)?);
                formatted_text.push_str("\n\n");
            }
        }
        
        if !other.is_empty() {
            formatted_text.push_str("=== ADDITIONAL CONTEXT ===\n\n");
            for bc in other {
                let llm_content = self.extract_llm_content(bc.id).await?;
                formatted_text.push_str(&self.format_content(llm_content)?);
                formatted_text.push_str("\n\n");
            }
        }
        
        // Recalculate token estimate based on formatted text
        let token_estimate = formatted_text.len() / 3;
        
        // Build context payload
        let context_payload = serde_json::json!({
            "consumer_id": consumer_id,
            "trigger_event_id": trigger_id,
            "assembled_at": chrono::Utc::now().to_rfc3339(),
            "token_estimate": token_estimate,
            "sources_assembled": context.sources_count,
            "formatted_context": formatted_text,
            "breadcrumb_count": context.breadcrumbs.len(),
        });
        
        // Check for existing context breadcrumb
        let consumer_tag = format!("consumer:{}", consumer_id);
        let existing = self.rcrt_client.search_breadcrumbs(
            "agent.context.v1",
            Some(vec![session_tag.to_string(), consumer_tag.clone()]),
        ).await?;
        
        if let Some(existing_bc) = existing.first() {
            // Update existing
            self.rcrt_client.update_breadcrumb(
                existing_bc.id,
                existing_bc.version,
                context_payload,
            ).await?;
        } else {
            // Create new
            self.rcrt_client.create_breadcrumb(
                "agent.context.v1",
                &format!("Context for {}", consumer_id),
                vec![
                    "agent:context".to_string(),
                    consumer_tag,
                    session_tag.to_string(),
                ],
                context_payload,
            ).await?;
        }
        
        tracing::info!("✅ Published two-phase context: trigger (full) + {} supporting breadcrumbs (~{} tokens)", 
            context.breadcrumbs.len(), token_estimate);
        
        Ok(())
    }
    
    /// Format llm_content into human-readable string
    fn format_content(&self, llm_content: serde_json::Value) -> Result<String> {
        match llm_content {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                Ok(serde_json::to_string_pretty(&llm_content)?)
            },
            _ => Ok(llm_content.to_string())
        }
    }
}

// REMOVED: categorize_schema() - No longer needed
// Breadcrumbs self-describe their formatting via llm_hints templates

