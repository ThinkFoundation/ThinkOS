# RCRT Quick Reference Guide

## System at a Glance

**RCRT** = Right Context Right Time - Event-driven AI agent orchestration platform

**Core Concept**: Everything is a **Breadcrumb** (versioned data with schema)

**Communication**: SSE (Server-Sent Events) for real-time pub/sub

**Routing**: Selector-based subscriptions (tags, schemas, context matching)

---

## Component Ports

| Component | Port | Purpose |
|-----------|------|---------|
| rcrt-server | 8081 | Main API & SSE |
| Dashboard v2 | 8082 | Web UI |
| PostgreSQL | 5432 | Database |
| NATS | 4222 | Pub/sub |
| Extension | - | Chrome sidepanel |

---

## Quick Start Commands

### Start Full Stack
```bash
# 1. Database
docker run -d -p 5432:5432 pgvector/pgvector:pg16 \
  -e POSTGRES_PASSWORD=postgres

# 2. NATS
docker run -d -p 4222:4222 nats:latest

# 3. Server (from project root)
export DB_URL=postgresql://postgres:postgres@localhost/rcrt
cargo run -p rcrt-server

# 4. Agent Runner
cd rcrt-visual-builder/apps/agent-runner
npm run dev

# 5. Tools Runner
cd rcrt-visual-builder/apps/tools-runner
npm run dev

# 6. Dashboard
cd rcrt-dashboard-v2/frontend
npm run dev

# 7. Extension (build and load in Chrome)
cd extension
npm run build
# Load dist/ folder in Chrome extensions
```

### Get JWT Token
```bash
curl -X POST http://localhost:8081/auth/token \
  -H "Content-Type: application/json" \
  -d '{
    "owner_id": "00000000-0000-0000-0000-000000000001",
    "agent_id": "00000000-0000-0000-0000-000000000AAA",
    "roles": ["curator", "emitter", "subscriber"]
  }'

# Save token
export TOKEN="eyJ..."
```

---

## Common API Calls

### Create Breadcrumb
```bash
curl -X POST http://localhost:8081/breadcrumbs \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Test Message",
    "description": "A test user message for demonstration",
    "schema_name": "user.message.v1",
    "tags": ["user:message", "test"],
    "context": {
      "content": "Hello RCRT!",
      "conversation_id": "conv-123"
    }
  }'
```

**New in v2.1.0:**
- `description` (optional) - Top-level detailed description
- `semantic_version` (optional) - Top-level version like "2.0.0"
- `llm_hints` (optional) - Top-level LLM optimization (exclude-only format in v2.2.0)

**llm_hints format (v2.2.0):**
```json
{
  "exclude": ["field1", "field2"]  // Required, can be empty []
}
```

### Search Breadcrumbs
```bash
# By tag
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8081/breadcrumbs?tag=user:message"

# By schema
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8081/breadcrumbs?schema_name=agent.def.v1"

# Vector search
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8081/breadcrumbs/search?q=hello&nn=5"
```

### Connect to SSE
```bash
# Browser/JavaScript
const eventSource = new EventSource(
  `http://localhost:8081/events/stream?token=${TOKEN}`
);

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.type !== 'ping') {
    console.log('Event:', data);
  }
};
```

### Update Breadcrumb
```bash
# Get current version first
VERSION=$(curl -s -H "Authorization: Bearer $TOKEN" \
  http://localhost:8081/breadcrumbs/$ID | jq -r '.version')

# Update with version check
curl -X PATCH http://localhost:8081/breadcrumbs/$ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "If-Match: $VERSION" \
  -d '{
    "context": {
      "content": "Updated content"
    }
  }'
```

---

## Common Breadcrumb Schemas

### User Message
```json
{
  "schema_name": "user.message.v1",
  "title": "User Question",
  "tags": ["user:message"],
  "context": {
    "content": "What is the weather?",
    "conversation_id": "conv-123",
    "timestamp": "2024-01-01T00:00:00Z"
  }
}
```

### Agent Response
```json
{
  "schema_name": "agent.response.v1",
  "title": "Agent Reply",
  "tags": ["agent:response"],
  "context": {
    "content": "The weather is sunny.",
    "conversation_id": "conv-123",
    "confidence": 0.95,
    "responding_to": "breadcrumb-id"
  }
}
```

### Tool Request
```json
{
  "schema_name": "tool.request.v1",
  "title": "Call OpenRouter",
  "tags": ["tool:request", "workspace:tools"],
  "context": {
    "tool": "openrouter",
    "input": {
      "messages": [{"role": "user", "content": "Hello"}],
      "model": "openai/gpt-4"
    },
    "requestId": "req-123",
    "requestedBy": "agent-uuid"
  }
}
```

### Tool Response
```json
{
  "schema_name": "tool.response.v1",
  "title": "OpenRouter Response",
  "tags": ["tool:response", "request:req-123"],
  "context": {
    "request_id": "req-123",
    "tool": "openrouter",
    "status": "success",
    "output": {
      "content": "Hello! How can I help?"
    },
    "execution_time_ms": 1234
  }
}
```

### Agent Definition (v2.1.0 Structure)
```json
{
  "schema_name": "agent.def.v1",
  "title": "Chat Agent",
  "description": "Helpful chat assistant with tool access",
  "semantic_version": "1.0.0",
  "tags": ["workspace:agents"],
  "context": {
    "agent_id": "agent-uuid",
    "llm_config_id": "config-uuid",
    "system_prompt": "You are a helpful assistant.",
    "capabilities": {
      "can_create_breadcrumbs": true,
      "can_update_own": true,
      "can_delete_own": false
    },
    "subscriptions": {
      "selectors": [
        {
          "schema_name": "agent.context.v1",
          "all_tags": ["consumer:my-agent"],
          "role": "trigger"
        }
      ]
    }
  }
}
```

### Tool Definition (v2.2.0 Structure - Exclude-Only)
```json
{
  "schema_name": "tool.code.v1",
  "title": "Calculator Tool",
  "description": "Perform mathematical calculations",
  "semantic_version": "2.0.0",
  "tags": ["tool", "tool:calculator", "workspace:tools"],
  "llm_hints": {
    "exclude": ["code", "permissions", "limits", "ui_schema"]
  },
  "context": {
    "name": "calculator",
    "code": {"language": "typescript", "source": "..."},
    "input_schema": {...},
    "output_schema": {...}
  }
---

## Selector Examples

### Match Any Tag
```json
{
  "any_tags": ["user:message", "agent:message"]
}
```

### Match All Tags
```json
{
  "all_tags": ["urgent", "customer", "priority:high"]
}
```

### Match Schema
```json
{
  "schema_name": "tool.response.v1"
}
```

### Match Context
```json
{
  "context_match": [
    {
      "path": "$.status",
      "op": "eq",
      "value": "error"
    },
    {
      "path": "$.priority",
      "op": "gt",
      "value": 5
    }
  ]
}
```

### Combined
```json
{
  "schema_name": "user.message.v1",
  "any_tags": ["urgent"],
  "context_match": [
    {
      "path": "$.conversation_id",
      "op": "eq",
      "value": "conv-123"
    }
  ]
}
```

---

## Environment Variables Cheatsheet

### rcrt-server
```bash
# Required
DB_URL=postgresql://user:pass@host/db
OWNER_ID=00000000-0000-0000-0000-000000000001

# Optional
NATS_URL=nats://localhost:4222
AUTH_MODE=jwt  # or 'disabled'
JWT_PUBLIC_KEY_PEM=...
JWT_PRIVATE_KEY_PEM=...
EMBED_MODEL=models/model.onnx
EMBED_TOKENIZER=models/tokenizer.json
HYGIENE_ENABLED=true
HYGIENE_INTERVAL_SECONDS=300
```

### agent-runner
```bash
RCRT_BASE_URL=http://localhost:8081
WORKSPACE=workspace:agents
OWNER_ID=00000000-0000-0000-0000-000000000001
AGENT_ID=00000000-0000-0000-0000-000000000AAA
OPENROUTER_API_KEY=sk-or-...
```

### tools-runner
```bash
RCRT_BASE_URL=http://localhost:8081
WORKSPACE=workspace:tools
DEPLOYMENT_MODE=local  # docker, local, electron
RCRT_AUTH_MODE=jwt  # disabled, jwt
```

### Dashboard v2
```bash
VITE_RCRT_BASE_URL=http://localhost:8081
```

---

## Debugging Tips

### Check Server Health
```bash
curl http://localhost:8081/health
# Should return: ok
```

### View Metrics
```bash
curl http://localhost:8081/metrics
# Prometheus-format metrics
```

### List All Breadcrumbs
```bash
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8081/breadcrumbs?limit=100"
```

### Check Agents
```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:8081/agents
```

### Manual Hygiene Run
```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:8081/hygiene/run
```

### SSE Test (curl)
```bash
curl -N -H "Authorization: Bearer $TOKEN" \
  http://localhost:8081/events/stream
```

### View Logs
```bash
# Server logs
RUST_LOG=debug cargo run -p rcrt-server

# Agent runner logs
cd rcrt-visual-builder/apps/agent-runner
npm run dev

# Tools runner logs
cd rcrt-visual-builder/apps/tools-runner
npm run dev
```

---

## Common Patterns

### Create Agent (v2.1.0)
```typescript
// 1. Create agent definition breadcrumb
const agentDef = await client.createBreadcrumb({
  schema_name: 'agent.def.v1',
  title: 'My Chat Agent',
  description: 'Helpful chat assistant with tool access',
  semantic_version: '1.0.0',
  tags: ['agent:def', 'workspace:agents'],
  context: {
    agent_id: uuidv4(),
    llm_config_id: null, // Set via Dashboard UI
    system_prompt: 'You are a helpful assistant...',
    capabilities: {
      can_create_breadcrumbs: true,
      can_use_tools: true
    },
    subscriptions: {
      selectors: [
        {
          schema_name: 'agent.context.v1',
          all_tags: ['consumer:my-agent'],
          role: 'trigger'
        }
      ]
    }
  }
});

// 2. Agent-runner auto-loads and starts the agent
```

### Create Tool (v2.2.0 - Exclude-Only)
```typescript
// 1. Create tool.code.v1 breadcrumb with Deno code
const toolDef = await client.createBreadcrumb({
  schema_name: 'tool.code.v1',
  title: 'My Tool',
  description: 'Does something useful',
  semantic_version: '2.0.0',
  tags: ['tool', 'tool:my-tool', 'workspace:tools'],
  llm_hints: {
    exclude: ['code', 'permissions', 'limits', 'ui_schema']
  },
  context: {
    name: 'my-tool',
    code: {
      language: 'typescript',
      source: 'export async function execute(input, context) { return {...}; }'
    },
    input_schema: {...},
    output_schema: {...}
  }
});

// 2. Tools-runner auto-loads the tool
```

### Request-Response Pattern
```typescript
// 1. Create request
const request = await client.createBreadcrumb({
  schema_name: 'tool.request.v1',
  tags: ['tool:request', 'workspace:tools'],
  context: {
    tool: 'openrouter',
    input: { messages: [...] },
    requestId: 'req-123'
  }
});

// 2. Wait for response (EventBridge)
const response = await eventBridge.waitForEvent({
  schema_name: 'tool.response.v1',
  request_id: 'req-123'
}, 60000);

// 3. Process response
console.log(response.context.output);
```

### Streaming Responses
```typescript
// 1. Connect to SSE
const cleanup = await client.connectToSSE(
  { tags: ['agent:response'] },
  (event) => {
    if (event.breadcrumb_id) {
      const breadcrumb = await client.getBreadcrumb(event.breadcrumb_id);
      console.log('Response:', breadcrumb.context.content);
    }
  }
);

// 2. Cleanup when done
cleanup();
```

---

## TypeScript SDK Examples

### Initialize Client
```typescript
import { RcrtClientEnhanced } from '@rcrt-builder/sdk';

const client = new RcrtClientEnhanced(
  'http://localhost:8081',
  'jwt',
  token,
  {
    autoRefresh: true,
    tokenEndpoint: 'http://localhost:8081/auth/token'
  }
);
```

### Create Breadcrumb
```typescript
const breadcrumb = await client.createBreadcrumb({
  title: 'My Breadcrumb',
  schema_name: 'custom.v1',
  tags: ['my-tag'],
  context: {
    key: 'value'
  },
  ttl: new Date(Date.now() + 3600000) // 1 hour
});
```

### Search Breadcrumbs
```typescript
// By tag
const messages = await client.searchBreadcrumbs({
  tag: 'user:message',
  limit: 10
});

// By schema with context
const agents = await client.searchBreadcrumbsWithContext({
  schema_name: 'agent.def.v1',
  include_context: true
});

// Vector search
const results = await client.vectorSearch({
  query: 'hello world',
  limit: 5,
  tag: 'user:message'
});
```

### Update Breadcrumb
```typescript
const current = await client.getBreadcrumb(id);

await client.updateBreadcrumb(
  id,
  current.version,
  {
    context: {
      ...current.context,
      updated: true
    }
  }
);
```

### SSE Connection
```typescript
const cleanup = await client.connectToSSE(
  {
    tags: ['agent:response'],
    schema_names: ['tool.response.v1']
  },
  async (event) => {
    console.log('Event:', event);
    
    if (event.breadcrumb_id) {
      const breadcrumb = await client.getBreadcrumb(event.breadcrumb_id);
      // Process breadcrumb
    }
  }
);

// Later: cleanup()
```

---

## Chrome Extension API

### Send Message
```typescript
import { rcrtClient } from '../lib/rcrt-client';

// Authenticate
await rcrtClient.authenticate();

// Create message
const result = await rcrtClient.createChatBreadcrumb({
  content: 'Hello!',
  sessionId: conversationId
});
```

### Listen for Responses
```typescript
const cleanup = await rcrtClient.listenForAgentResponses(
  conversationId,
  (content) => {
    console.log('Agent response:', content);
  }
);
```

---

## Troubleshooting Checklist

### Agent Not Responding
- [ ] Agent definition breadcrumb exists
- [ ] Agent-runner is running
- [ ] Subscriptions match event tags/schema
- [ ] JWT token is valid
- [ ] Check agent-runner logs

### Tool Not Executing
- [ ] Tool definition breadcrumb exists
- [ ] Tools-runner is running
- [ ] tool.request.v1 has correct context.tool name
- [ ] Check tools-runner logs
- [ ] Verify secrets if tool needs them

### SSE Connection Issues
- [ ] JWT token valid and not expired
- [ ] CORS enabled on rcrt-server
- [ ] NATS running (if using NATS feature)
- [ ] Check browser console for errors
- [ ] Try token in query param: ?token=...

### Breadcrumb Not Found
- [ ] Breadcrumb actually created (check response)
- [ ] Using correct breadcrumb ID
- [ ] Owner ID matches
- [ ] No visibility restrictions
- [ ] Check database directly if needed

### Vector Search Returns Nothing
- [ ] Embeddings enabled for schema
- [ ] ONNX model loaded (check server logs)
- [ ] Try with different query
- [ ] Check if breadcrumbs have embeddings (embedding column)

---

## Performance Tuning

### Database Optimization
```sql
-- Create indexes
CREATE INDEX idx_breadcrumbs_tags ON breadcrumbs USING GIN(tags);
CREATE INDEX idx_breadcrumbs_schema ON breadcrumbs(schema_name);
CREATE INDEX idx_breadcrumbs_owner ON breadcrumbs(owner_id);

-- HNSW index for vector search
CREATE INDEX idx_breadcrumbs_embedding ON breadcrumbs 
  USING hnsw (embedding vector_cosine_ops);
```

### Hygiene Configuration
```bash
# More aggressive cleanup
HYGIENE_INTERVAL_SECONDS=60  # Every minute
HYGIENE_HEALTHCHECK_TTL_MINUTES=1
HYGIENE_TEMP_DATA_TTL_HOURS=1

# Less aggressive (production)
HYGIENE_INTERVAL_SECONDS=300  # Every 5 minutes
HYGIENE_HEALTHCHECK_TTL_MINUTES=5
HYGIENE_TEMP_DATA_TTL_HOURS=24
```

### Connection Pooling
```bash
# In DB_URL
postgresql://user:pass@host/db?max_connections=20
```

---

## Security Best Practices

1. **Use JWT in Production**
   ```bash
   # Generate RSA keys
   openssl genrsa -out private.pem 2048
   openssl rsa -in private.pem -pubout -out public.pem
   
   # Set environment variables
   JWT_PRIVATE_KEY_PEM=$(cat private.pem)
   JWT_PUBLIC_KEY_PEM=$(cat public.pem)
   ```

2. **Rotate Tokens Regularly**
   ```typescript
   // Set short TTL
   ttl_sec: 3600  // 1 hour
   
   // Auto-refresh
   client.setAutoRefresh(true);
   ```

3. **Use Secrets for API Keys**
   ```bash
   curl -X POST http://localhost:8081/secrets \
     -H "Authorization: Bearer $TOKEN" \
     -d '{
       "name": "OPENROUTER_API_KEY",
       "scope_type": "global",
       "value": "sk-or-..."
     }'
   ```

4. **Enable HMAC for Webhooks**
   ```typescript
   // Server calculates signature
   X-RCRT-Signature: sha256=abc123...
   
   // Client verifies
   const hmac = crypto.createHmac('sha256', secret);
   hmac.update(body);
   const signature = hmac.digest('hex');
   ```

5. **Set Appropriate Visibility**
   ```json
   {
     "visibility": "private",  // Only owner
     "sensitivity": "pii"      // Contains personal data
   }
   ```

---

## Useful Links

- **API Docs**: http://localhost:8081/docs (Redoc)
- **Swagger UI**: http://localhost:8081/swagger
- **Dashboard v2**: http://localhost:8082
- **Metrics**: http://localhost:8081/metrics
- **Health**: http://localhost:8081/health

---

**Quick Reference Version**: 1.0  
**Last Updated**: 2024  
**For Full Documentation**: See SYSTEM_ARCHITECTURE_OVERVIEW.md
