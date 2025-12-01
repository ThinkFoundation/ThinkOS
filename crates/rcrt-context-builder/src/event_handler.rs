/*!
 * Event Handler
 * 
 * Main event loop that processes SSE events and triggers context assembly
 * UNIVERSAL: Uses pointer-based context assembly for ALL agents
 */

use crate::{
    config::Config,
    rcrt_client::{RcrtClient, BreadcrumbEvent},
    vector_store::VectorStore,
    graph::SessionGraphCache,
    retrieval::ContextAssembler,
    output::ContextPublisher,
    entity_extractor::EntityExtractor,
};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn, error};

pub struct EventHandler {
    rcrt_client: Arc<RcrtClient>,
    vector_store: Arc<VectorStore>,
    graph_cache: Arc<SessionGraphCache>,
    assembler: ContextAssembler,
    publisher: ContextPublisher,
    entity_extractor: Arc<EntityExtractor>,
    config: Config,
}

impl EventHandler {
    pub fn new(
        rcrt_client: Arc<RcrtClient>,
        vector_store: Arc<VectorStore>,
        graph_cache: Arc<SessionGraphCache>,
        entity_extractor: Arc<EntityExtractor>,
        config: Config,
    ) -> Self {
        let assembler = ContextAssembler::new(vector_store.clone());
        let publisher = ContextPublisher::new(rcrt_client.clone());
        
        EventHandler {
            rcrt_client,
            vector_store,
            graph_cache,
            assembler,
            publisher,
            entity_extractor,
            config,
        }
    }
    
    pub async fn start(&self) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        
        // Start SSE stream
        self.rcrt_client.start_sse_stream(tx).await?;
        
        info!("✅ Event handler started, listening for events...");
        
        // Process events
        while let Some(event) = rx.recv().await {
            if let Err(e) = self.handle_event(event).await {
                error!("Error handling event: {}", e);
            }
        }
        
        Ok(())
    }
    
    async fn handle_event(&self, event: BreadcrumbEvent) -> Result<()> {
        // UNIVERSAL: Process ANY schema using pointer-based context assembly
        // Zero hardcoding - fully data-driven
        
        let Some(schema) = &event.schema_name else { return Ok(()); };
        
        info!("📨 Event received: {} (id: {:?})", schema, event.breadcrumb_id);
        
        // Find ALL agents that want context for this trigger
        let interested_agents = self.find_agents_for_trigger(schema, event.tags.as_ref()).await?;
        
        if interested_agents.is_empty() {
            // No agents need context assembly for this schema
            // Normal - tools handle requests directly without context-builder
            return Ok(());
        }
        
        info!("🎯 {} agent(s) want context for {}", interested_agents.len(), schema);
        
        // Extract session tag (universal across all schemas)
        let session_tag = event.tags.as_ref()
            .and_then(|tags| tags.iter().find(|t| t.starts_with("session:")))
            .cloned();
        
        // Assemble context for EACH interested agent
        for agent_def in interested_agents {
            info!("🔄 Assembling context for {}", agent_def.agent_id);
            
            // UNIVERSAL assembly with pointers
            self.assemble_with_pointers(
                &agent_def.agent_id,
                event.breadcrumb_id,
                &agent_def,
                session_tag.as_deref()
            ).await?;
        }
        
        Ok(())
    }
    
    // ============ UNIVERSAL POINTER-BASED CONTEXT ASSEMBLY ============
    
    /// Find ALL agents that want context assembled for this trigger
    /// Pure data-driven - queries agent.def.v1 for matching context_trigger
    async fn find_agents_for_trigger(
        &self,
        trigger_schema: &str,
        trigger_tags: Option<&Vec<String>>,
    ) -> Result<Vec<crate::agent_config::AgentDefinition>> {
        use crate::agent_config::load_all_agent_definitions_with_triggers;
        
        // Load ALL agent definitions that declare context_trigger
        let all_agents = load_all_agent_definitions_with_triggers(self.vector_store.pool()).await?;
        
        // Filter to agents whose context_trigger matches this event
        let matching: Vec<_> = all_agents.into_iter()
            .filter(|agent| {
                if let Some(trigger_config) = &agent.context_trigger {
                    // Schema must match
                    if trigger_config.schema_name != trigger_schema {
                        return false;
                    }
                    
                    // none_tags - if ANY of these tags present, EXCLUDE (loop prevention)
                    if let Some(excluded_tags) = &trigger_config.none_tags {
                        if let Some(event_tags) = trigger_tags {
                            if excluded_tags.iter().any(|t| event_tags.contains(t)) {
                                return false;  // Exclude this agent - has excluded tag
                            }
                        }
                    }
                    
                    // all_tags must ALL be present
                    if let Some(required_tags) = &trigger_config.all_tags {
                        if let Some(event_tags) = trigger_tags {
                            return required_tags.iter().all(|t| event_tags.contains(t));
                        }
                        return false;
                    }
                    
                    // any_tags at least ONE must be present
                    if let Some(any_of_tags) = &trigger_config.any_tags {
                        if let Some(event_tags) = trigger_tags {
                            return any_of_tags.iter().any(|t| event_tags.contains(t));
                        }
                        return false;
                    }
                    
                    // No tag requirements - schema match is enough
                    true
                } else {
                    false
                }
            })
            .collect();
        
        Ok(matching)
    }
    
    /// Universal context assembly using hybrid pointers
    /// Works for ALL agents - zero hardcoding
    async fn assemble_with_pointers(
        &self,
        consumer_id: &str,
        trigger_id: Option<uuid::Uuid>,
        agent_def: &crate::agent_config::AgentDefinition,
        session_tag: Option<&str>,
    ) -> Result<()> {
        use crate::retrieval::AssembledContext;
        
        let Some(trigger) = trigger_id else { 
            warn!("No trigger ID for {}, skipping", consumer_id);
            return Ok(()); 
        };
        
        // STEP 1: GET TRIGGER BREADCRUMB
        let trigger_bc = self.vector_store.get_by_id(trigger).await?
            .ok_or_else(|| anyhow::anyhow!("Trigger breadcrumb not found"))?;
        
        info!("🌱 Extracting hybrid pointers from trigger...");
        
        // STEP 2: EXTRACT HYBRID POINTERS (tags + cached keywords)
        let mut pointers = Vec::new();
        
        // From tags (explicit pointer tags)
        for tag in &trigger_bc.tags {
            if !tag.contains(':') && !Self::is_state_tag(tag) {
                pointers.push(tag.to_lowercase());
            }
        }
        
        // From cached entity_keywords (pre-extracted at creation)
        if let Some(keywords) = &trigger_bc.entity_keywords {
            pointers.extend(keywords.iter().cloned());
        }
        
        // Deduplicate
        pointers.sort();
        pointers.dedup();
        
        info!("📍 Extracted {} pointers: {:?}", 
            pointers.len(), 
            &pointers[..pointers.len().min(10)]
        );
        
        // STEP 3: COLLECT SEEDS (multi-source)
        let mut seed_ids = vec![trigger];
        info!("🌱 Collecting seed nodes...");
        info!("  + Seed: trigger");
        
        // Always sources (from agent.def.v1.context_sources.always)
        if let Some(context_sources) = &agent_def.context_sources {
            if let Some(always) = &context_sources.always {
                for source in always {
                    let nodes_result = match source.source_type.as_str() {
                        "schema" => {
                            if let Some(schema_name) = &source.schema_name {
                                self.fetch_by_schema(schema_name, source.method.as_deref(), source.limit.unwrap_or(1)).await
                            } else {
                                continue;
                            }
                        },
                        "tag" => {
                            if let Some(tag) = &source.tag {
                                self.fetch_by_tag(tag, source.limit.unwrap_or(1)).await
                            } else {
                                continue;
                            }
                        },
                        _ => continue,
                    };
                    
                    if let Ok(nodes) = nodes_result {
                        for node in nodes {
                            if !seed_ids.contains(&node.id) {
                                seed_ids.push(node.id);
                                info!("  + Seed: {} (always source)", 
                                    source.schema_name.as_deref().or(source.tag.as_deref()).unwrap_or("unknown"));
                            }
                        }
                    }
                }
            }
            
            // Semantic sources (using hybrid pointers!)
            if let Some(semantic) = &context_sources.semantic {
                if semantic.enabled && !pointers.is_empty() {
                    info!("🔍 Semantic search with {} pointers", pointers.len());
                    
                    for schema in &semantic.schemas {
                        if let Some(embedding) = &trigger_bc.embedding {
                            let semantic_seeds = self.vector_store.find_similar_hybrid(
                                embedding,
                                &pointers,  // Hybrid pointers!
                                semantic.limit.unwrap_or(3),
                                None  // Global search for knowledge
                            ).await?;
                            
                            for seed in semantic_seeds {
                                if !seed_ids.contains(&seed.id) {
                                    seed_ids.push(seed.id);
                                }
                            }
                            info!("  + Seeds: semantic+pointers ({})", schema);
                        }
                    }
                }
            }
        }
        
        // Session messages (temporal context)
        if let Some(session) = session_tag {
            let recent = self.vector_store.get_recent(
                None,  // All schemas
                Some(session),
                20  // Last 20 in session
            ).await?;
            
            for row in recent {
                if !seed_ids.contains(&row.id) {
                    seed_ids.push(row.id);
                }
            }
            info!("  + Seeds: session messages");
        }
        
        info!("✅ Collected {} total seeds", seed_ids.len());
        
        // STEP 4: LOAD GRAPH around seeds
        let graph = self.load_graph_for_seeds(&seed_ids).await?;
        
        // STEP 5: PATHFINDER with token budget
        let llm_config = crate::llm_config::load_llm_config(
            agent_def.llm_config_id.clone(),
            self.vector_store.pool()
        ).await?;
        
        let token_budget_info = crate::llm_config::calculate_context_budget(
            &llm_config,
            self.vector_store.pool()
        ).await?;
        
        info!("💰 Context budget: {} tokens", token_budget_info.tokens);
        
        let path_finder = crate::retrieval::PathFinder::new(5, 50);
        let relevant_ids = path_finder.find_paths_token_aware(
            &graph,
            seed_ids.clone(),
            token_budget_info.tokens
        );
        
        info!("✅ PathFinder selected {} nodes", relevant_ids.len());
        
        // STEP 6: Fetch breadcrumbs and format
        let mut breadcrumbs = Vec::new();
        for node_id in relevant_ids {
            if let Some(node) = graph.nodes.get(&node_id) {
                breadcrumbs.push(node.clone());
            }
        }
        
        // Sort by priority
        breadcrumbs.sort_by(|a, b| {
            let a_priority = schema_priority(&a.schema_name);
            let b_priority = schema_priority(&b.schema_name);
            
            if a_priority != b_priority {
                a_priority.cmp(&b_priority)
            } else {
                b.created_at.cmp(&a.created_at)
            }
        });
        
        let token_estimate: usize = breadcrumbs.iter()
            .map(|bc| bc.context.to_string().len() / 3)
            .sum();
        
        let context = AssembledContext {
            breadcrumbs,
            token_estimate,
            sources_count: seed_ids.len(),
        };
        
        // STEP 7: PUBLISH
        self.publisher.publish_context(
            consumer_id,
            session_tag.unwrap_or(""),
            Some(trigger),
            &context
        ).await?;
        
        info!("✅ Context published for {}", consumer_id);
        
        Ok(())
    }
    
    // ============ HELPER FUNCTIONS ============
    
    /// Load graph for seed breadcrumbs
    /// NOTE: breadcrumb_edges table was removed in migration 0012 (GNN cleanup)
    /// Now returns a simple graph with only seed nodes, no edges
    async fn load_graph_for_seeds(&self, seed_ids: &[uuid::Uuid]) -> Result<crate::graph::SessionGraph> {
        use crate::graph::SessionGraph;
        
        let mut graph = SessionGraph::new(String::new());  // Generic graph, not session-specific
        
        // Add seed nodes only (no edges - breadcrumb_edges table removed)
        for &seed_id in seed_ids {
            if let Some(bc) = self.vector_store.get_by_id(seed_id).await? {
                graph.add_node(breadcrumb_row_to_node(bc));
            }
        }
        
        Ok(graph)
    }
    
    /// Check if tag is a state tag (not a pointer)
    fn is_state_tag(tag: &str) -> bool {
        matches!(tag,
            "approved" | "validated" | "bootstrap" | "deprecated" |
            "draft" | "archived" | "ephemeral" | "error" | "warning" | "info"
        )
    }
    
    /// Fetch breadcrumbs by schema
    async fn fetch_by_schema(
        &self,
        schema_name: &str,
        method: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::vector_store::BreadcrumbRow>> {
        match method.unwrap_or("latest") {
            "latest" => {
                if let Some(row) = self.vector_store.get_latest(schema_name, None).await? {
                    Ok(vec![row])
                } else {
                    Ok(vec![])
                }
            },
            "recent" | "all" => {
                self.vector_store.get_recent(Some(schema_name), None, limit).await
            },
            _ => Ok(vec![])
        }
    }
    
    /// Fetch breadcrumbs by tag
    async fn fetch_by_tag(
        &self,
        tag: &str,
        limit: usize,
    ) -> Result<Vec<crate::vector_store::BreadcrumbRow>> {
        self.vector_store.get_by_tag(tag, limit).await
    }
}

// ============ FREE HELPER FUNCTIONS ============

/// Schema priority for sorting context
fn schema_priority(schema: &str) -> u8 {
    match schema {
        "tool.code.v1" => 1,  // Individual tools first (direct discovery, no aggregation!)
        "agent.catalog.v1" => 2,  // Then agents
        "browser.tab.context.v1" => 3,  // Then browser context
        "knowledge.v1" => 4,  // Then knowledge
        "user.message.v1" | "agent.response.v1" => 5,  // Then messages
        "tool.catalog.v1" => 99,  // Deprecated - use tool.code.v1 instead
        _ => 10  // Everything else last
    }
}

/// Convert BreadcrumbRow to BreadcrumbNode
fn breadcrumb_row_to_node(row: crate::vector_store::BreadcrumbRow) -> crate::graph::BreadcrumbNode {
    let trigger_event_id = row.context
        .get("trigger_event_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    
    crate::graph::BreadcrumbNode {
        id: row.id,
        schema_name: row.schema_name,
        tags: row.tags,
        context: row.context,
        embedding: row.embedding,
        created_at: row.created_at,
        trigger_event_id,
    }
}

/// Wrapper for EventHandler::is_state_tag (for external use)
fn is_state_tag(tag: &str) -> bool {
    EventHandler::is_state_tag(tag)
}

