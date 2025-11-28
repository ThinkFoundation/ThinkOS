# RCRT Core Principles & Primitives

**Version:** 3.0 - Definitive Specification  
**Philosophy:** Simple, well-built primitives enable complex emergent functionality

---

## The Three Primitives

RCRT is built on exactly **three primitives** - nothing more, nothing less:

### 1. Breadcrumbs
**Persistent, versioned data packets**

```typescript
{
  id: UUID,
  title: string,
  context: object,      // Flexible JSON
  tags: string[],       // Universal primitive
  schema_name?: string,
  version: number,      // Optimistic locking
  embedding?: vector,   // 384-dim for semantic search
  ttl?: timestamp       // Auto-expiry
}
```

**Properties:**
- Immutable history (versioned)
- Searchable (tags + vector)
- Observable (events on change)
- Expirable (TTL)

### 2. Events
**Real-time breadcrumb change notifications**

```typescript
{
  type: "breadcrumb.updated",
  breadcrumb_id: UUID,
  schema_name: string,
  tags: string[],
  version: number,
  timestamp: ISO8601
}
```

**Delivery:**
- SSE (server → clients)
- NATS (service → service)
- Global broadcast, client-side filtering

### 3. Tags
**Universal routing + pointers + state**

```
workspace:tools      → Routing (namespace:id)
browser-automation   → Pointer (semantic keyword)
approved             → State (lifecycle)
```

**Three tag types, three purposes:**
1. **Routing tags** (`namespace:id`) - Event subscription matching
2. **Pointer tags** (keywords) - Semantic search seeds (hybrid: 60% vector + 40% keywords)
3. **State tags** (lifecycle) - Permissions, filtering

**ONE primitive powers everything.**

---

## The Core Pattern: Fire-and-Forget

**The foundational execution model:**

```
Event arrives → Process → Create breadcrumb → EXIT
```

**NOT:**
- ❌ Wait for responses
- ❌ Poll for changes
- ❌ Hold state in memory
- ❌ Run continuous loops

**Why:**
- Stateless (restart anytime)
- Scalable (100+ parallel instances)
- Resilient (isolated failures)
- Observable (breadcrumb trail)

**No exceptions. Every service. Every time.**

---

## The Intelligence Multiplier: context-builder

**Why agents are intelligent:**

```
Trigger event (user.message.v1, etc.)
  ↓
context-builder assembles rich context:
  - Vector search (similar content)
  - Graph walk (2-hop expansion)
  - Pointer-based discovery (hybrid search)
  - Token-aware budgets (50K-750K)
  ↓
Creates agent.context.v1 (pre-assembled, LLM-optimized)
  ↓
Agent receives RICH context → Makes intelligent decisions
```

**WITHOUT context-builder:**
- Agent gets empty context
- No data to reason about
- FAILS

**This is not optional** - context-builder IS the intelligence layer.

---

## The Core Distinction: Agents vs Tools

### Agents = Context + Reasoning

**Requirements:**
- 🔴 MUST use context-builder
- 🔴 MUST subscribe to `agent.context.v1`
- 🔴 MUST orchestrate via `tool.request.v1`
- 🔴 MUST use fire-and-forget

**Use for:**
- Complex reasoning
- Multi-step orchestration
- Adaptive behavior
- Decision-making

### Tools = Data + Code

**Requirements:**
- Subscribe to `tool.request.v1`
- Execute function
- Return `tool.response.v1`
- Fire-and-forget

**Use for:**
- Deterministic functions
- External API calls
- Data transformations
- Atomic operations

**The test:** If it bypasses context-builder, it's NOT an agent.

---

## Core Data Flow

**Complete flow (12 steps):**

```
1. User input → user.message.v1
2. NATS broadcast
3. context-builder triggered
4. Assembles context (vector + graph + pointers)
5. Creates agent.context.v1
6. NATS broadcast
7. Agent triggered (1st invocation)
8. Creates tool.request.v1
9. tools-runner executes
10. Creates tool.response.v1
11. Agent triggered (2nd invocation)
12. Creates agent.response.v1
```

**Characteristics:**
- 5 separate service invocations
- 4 breadcrumbs created
- ~2-3 seconds total
- Fully observable
- Horizontally scalable

---

## Core Schemas

**Only these schemas are essential:**

### System
- `agent.def.v1` - Agent definitions
- `tool.code.v1` - Self-contained tools
- `tool.config.v1` - LLM configurations

### Execution
- `agent.context.v1` - Pre-assembled context
- `tool.request.v1` - Tool invocations
- `tool.response.v1` - Tool results
- `agent.response.v1` - Agent outputs

### Communication
- `user.message.v1` - User inputs
- `browser.tab.context.v1` - Browser state (TTL)

**Everything else is optional domain-specific extension.**

---

## Core Services

**9 services, clear responsibilities:**

1. **rcrt-server** (Rust) - Storage + API + SSE + NATS
2. **PostgreSQL + pgvector** - Persistent storage + vector search
3. **NATS** - Event bus
4. **context-builder** (Rust) - Intelligence layer (THE critical service)
5. **agent-runner** (TypeScript) - Agent execution
6. **tools-runner** (TypeScript) - Tool execution (Deno)
7. **dashboard** (React) - Admin UI
8. **extension** (TypeScript) - Browser integration
9. **bootstrap** (Node.js) - System initialization

**Dependency hierarchy:**
```
PostgreSQL → rcrt-server → context-builder → agent-runner
                        → tools-runner ↗
```

---

## Core Principles

### 1. Everything is a Breadcrumb
Not just user data - settings, sessions, configurations, UI state, everything.

### 2. Events Drive Everything
No polling, no waiting. Event-driven choreography (not orchestration).

### 3. Tags are Universal
Routing + Pointers + State in ONE primitive.

### 4. Fire-and-Forget Always
No exceptions. Every service. Every invocation.

### 5. Context-Builder is Intelligence
Agents without context-builder = broken. Non-negotiable.

### 6. Agents Orchestrate, Tools Execute
Clear separation. No mixing.

### 7. Observable by Default
Every operation creates breadcrumb trail.

### 8. Stateless Services
State lives in breadcrumbs (database), not memory.

---

## Anti-Patterns (Forbidden)

### ❌ Direct Agent Subscriptions
```
// WRONG
agent subscribes to domain.v1 directly
```

**Why forbidden:** Bypasses context-builder, gets empty context, fails.

**Correct:** Agent subscribes to `agent.context.v1` from context-builder.

### ❌ Agents Executing Code
```
// WRONG
agent directly calls OpenRouter API
```

**Why forbidden:** Agents are for reasoning, not execution.

**Correct:** Agent creates `tool.request.v1`, tools-runner executes.

### ❌ Waiting for Responses
```
// WRONG
const response = await callTool();
return response;
```

**Why forbidden:** Breaks fire-and-forget, creates coupling.

**Correct:** Create `tool.request.v1` and EXIT. Handle response in separate invocation.

### ❌ Polling
```
// WRONG
while (!done) {
  const status = await checkStatus();
  await sleep(1000);
}
```

**Why forbidden:** Events exist for this.

**Correct:** Subscribe to events, react when they arrive.

### ❌ Local State
```
// WRONG
const cache = new Map();
```

**Why forbidden:** Not horizontally scalable.

**Correct:** State in breadcrumbs, services stateless.

---

## Validation Checklist

**Is it RCRT-compliant?**

- [ ] Uses breadcrumbs for state (not memory)
- [ ] Uses events for communication (not polling)
- [ ] Uses tags for routing (not hardcoded paths)
- [ ] Fire-and-forget execution (no waiting)
- [ ] Agents use context-builder (no direct subscriptions)
- [ ] Agents orchestrate via tool.request.v1 (no direct execution)
- [ ] Services are stateless (can restart anytime)
- [ ] Creates observable breadcrumb trail

**All must be TRUE. No exceptions.**

---

## Emergence from Primitives

**What emerges from just 3 primitives:**

From **Breadcrumbs**:
- Versioning (optimistic locking)
- Search (tags + vector)
- Expiry (TTL)
- History (immutable)
- Collaboration (shared state)

From **Events**:
- Real-time updates
- Decoupled services
- Scalability
- Resilience
- Observability

From **Tags**:
- Event routing
- Semantic search
- State management
- Permissions
- Identity

**Complex emergent functionality from simple, well-built primitives.**

Like the internet: IP + TCP + HTTP → Everything.

RCRT: Breadcrumbs + Events + Tags → Intelligent agent orchestration.

---

## Summary

**RCRT in one sentence:**
Event-driven agent orchestration using breadcrumbs (data), events (communication), and tags (routing/search/state), with context-builder as the intelligence layer and fire-and-forget as the execution pattern.

**The non-negotiables:**
1. Three primitives only (breadcrumbs, events, tags)
2. Fire-and-forget execution
3. context-builder for agent intelligence
4. Stateless services
5. Observable by default

**Everything else is implementation detail or domain extension.**

---

**This is RCRT. Nothing more, nothing less.**
