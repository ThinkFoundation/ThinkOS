# RCRT Documentation

**Welcome to the RCRT documentation!** This folder contains all core system documentation.

---

## 🚀 Start Here

**New to RCRT?**
1. Read [CORE_PRINCIPLES.md](CORE_PRINCIPLES.md) - The three primitives (10 min) ⭐ **START HERE**
2. Read [SYSTEM_ARCHITECTURE.md](SYSTEM_ARCHITECTURE.md) **Executive Summary** - Quick orientation (5 min)
3. Read [../QUICK_START.md](../QUICK_START.md) - Get running in minutes (15 min)
4. Deep dive [SYSTEM_ARCHITECTURE.md](SYSTEM_ARCHITECTURE.md) - Complete system (2-3 hours)

> 💡 **Tip:** CORE_PRINCIPLES.md defines RCRT in ~300 lines. Everything else is implementation detail.

---

## 📚 Core Documentation

### System Design & Architecture

**[CORE_PRINCIPLES.md](CORE_PRINCIPLES.md)** ⭐ **START HERE FIRST**
- The three primitives (breadcrumbs, events, tags)
- Fire-and-forget pattern (THE foundational execution model)
- context-builder intelligence layer
- Agents vs tools distinction
- Core schemas (9 essential)
- Anti-patterns (forbidden)
- Validation checklist
- **Defines RCRT in ~300 lines - Read this first!**

**[SYSTEM_ARCHITECTURE.md](SYSTEM_ARCHITECTURE.md)** ⭐ **IMPLEMENTATION DETAILS**
- **Executive Summary** (5 min) - Quick orientation, status, critical patterns
- Complete system design (1,900+ lines total)
- All 9 services documented with I/O and patterns
- Complete data flows (12-step chat, 10-step notes)
- Event-driven communication (SSE, NATS, fire-and-forget)
- Breadcrumb system (schemas, TTL, llm_hints)
- Agents vs Tools (requirements + counter-examples)
- Current state (🟢 working, 🟡 limited, 🔴 broken)
- Performance, security, validation

**[RCRT_PRINCIPLES.md](RCRT_PRINCIPLES.md)** (Extended Examples)
- Core philosophy with detailed examples
- Design principles
- Metrics and performance data
- Advanced patterns

---

### Guides & Reference

**[QUICK_REFERENCE.md](QUICK_REFERENCE.md)**
- API cheatsheet
- Common operations
- Code examples

**[DEPLOYMENT.md](DEPLOYMENT.md)**
- Local development setup
- Docker Compose deployment
- Kubernetes (Helm)
- Production configuration

**[Integration_Guide.md](Integration_Guide.md)**
- API integration
- Authentication
- Event subscriptions
- Breadcrumb operations

**[BOOTSTRAP_SYSTEM.md](BOOTSTRAP_SYSTEM.md)**
- System initialization
- Bootstrap process
- Loading breadcrumbs

---

### API Specification

**[openapi.json](openapi.json)**
- OpenAPI 3.0 specification
- All endpoints documented
- Schemas and examples
- View at: http://localhost:8081/docs

---

## 🎯 By Use Case

**I want to understand RCRT:**
→ SYSTEM_ARCHITECTURE.md + RCRT_PRINCIPLES.md

**I want to get started:**
→ ../QUICK_START.md + DEPLOYMENT.md

**I want to integrate:**
→ Integration_Guide.md + QUICK_REFERENCE.md

**I want API details:**
→ openapi.json + QUICK_REFERENCE.md

---

## 📖 Documentation Philosophy

**This folder contains ONLY:**
- ✅ Core system architecture
- ✅ Essential guides
- ✅ API reference
- ✅ Known issues with solutions

**Component-specific docs live in component directories:**
- Extension: `rcrt-extension-v2/`
- Visual Builder: `rcrt-visual-builder/`
- Desktop: `desktop-build/`

**No temporary status files or work summaries** (use CHANGELOG.md for version history)

---

## 🔄 Keeping Docs Updated

**When adding features:**
- Update SYSTEM_ARCHITECTURE.md with new patterns
- Update CHANGELOG.md with changes
- Update component-specific docs if needed

**When fixing issues:**
- Document solution
- Update CHANGELOG.md
- Remove from known issues when fixed

**Keep it clean!** Only essential, evergreen documentation.

---

**Total docs in this folder:** 8 core files + 2 reference files  
**All consolidated from:** 92 files  
**Reduction:** 89%  

**Everything you need, nothing you don't.** ✅
