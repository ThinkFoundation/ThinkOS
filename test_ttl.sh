#!/bin/bash

# Get JWT token
echo "Getting JWT token..."
TOKEN_RESP=$(curl -s -X POST http://localhost:8081/auth/token \
  -H "Content-Type: application/json" \
  -d '{
    "owner_id": "00000000-0000-0000-0000-000000000001",
    "agent_id": "00000000-0000-0000-0000-0000000000aa",
    "roles": ["emitter", "curator"]
  }')

# Extract token using grep/sed (fallback if jq not present)
TOKEN=$(echo $TOKEN_RESP | grep -o '"token":"[^"]*"' | sed 's/"token":"//;s/"//')

if [ -z "$TOKEN" ]; then
  echo "Failed to get token: $TOKEN_RESP"
  exit 1
fi

echo "Got token: ${TOKEN:0:10}..."

# Create a breadcrumb with ttl:5min tag
echo "Creating breadcrumb with ttl:5min tag..."
curl -X POST http://localhost:8081/breadcrumbs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "title": "TTL Test 5min",
    "context": {"test": "ttl"},
    "tags": ["ttl:5min", "test:ttl"],
    "schema_name": "test.ttl.v1"
  }'

# Create a breadcrumb with health:check tag (should be 5 min auto)
echo -e "\nCreating breadcrumb with health:check tag..."
curl -X POST http://localhost:8081/breadcrumbs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "title": "TTL Test Health Check",
    "context": {"test": "health"},
    "tags": ["health:check", "test:ttl"],
    "schema_name": "tool.request.v1"
  }'

# Create a breadcrumb for categorization test (should get category:tool)
echo -e "\nCreating breadcrumb for categorization test..."
curl -X POST http://localhost:8081/breadcrumbs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "title": "Categorization Test",
    "context": {"test": "cat"},
    "tags": ["tool:test", "test:cat"],
    "schema_name": "tool.test.v1"
  }'

echo -e "\nDone. Check logs for TTL application."
