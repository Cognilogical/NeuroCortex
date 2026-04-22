#!/bin/bash
set -e

echo "=== Testing learn_behavioral_rule ==="
echo '{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "learn_behavioral_rule", "arguments": {"rule_class": "security", "trigger_pattern": "cat /root/secret", "constraint_text": "Never read from /root/secret. It is forbidden."}}}' | ./target/debug/neurocortex > learn_res.json
cat learn_res.json | jq .

echo -e "\n=== Testing local_guard_validate (with injected constraints) ==="
echo '{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "local_guard_validate", "arguments": {"action_type": "bash", "payload": "cat /root/secret", "orchestrator_id": "test", "trace_id": "2"}}}' | ./target/debug/neurocortex > validate_res.json

# Extract just the verdict string from the nested JSON to make it readable
cat validate_res.json | jq -r '.result.content[0].text' | jq .
