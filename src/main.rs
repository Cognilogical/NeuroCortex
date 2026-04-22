use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

mod models;
mod sandbox;
mod semantic;
pub mod embedded_llm;

use std::collections::HashMap;

use models::{BehavioralRule, ValidateRequest, ValidateResponse, ValidateVerdict};
use sandbox::SandboxEvaluator;
use semantic::SemanticEvaluator;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    let sandbox_eval = SandboxEvaluator::new().await;
    let semantic_eval = SemanticEvaluator::new(".neurostrata".to_string())
        .await
        .unwrap_or_else(|_| panic!("Failed to connect to LanceDB"));

    // Track churn per unique action payload to prevent infinite agent lockups
    let mut churn_tracker: HashMap<String, u32> = HashMap::new();

    loop {
        line.clear();
        let n = stdin.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        if let Ok(req) = serde_json::from_str::<Value>(&line) {
            let method = req["method"].as_str().unwrap_or("");
            if method == "tools/call" {
                let params = &req["params"];
                let tool_name = params["name"].as_str().unwrap_or("");

                if tool_name == "local_guard_validate" {
                    let args = &params["arguments"];
                    let action_req: ValidateRequest = serde_json::from_value(args.clone())?;

                    // Prevent Infinite Churn: If the agent repeatedly tries the EXACT same action and it gets rejected,
                    // we must eventually fail-open so the orchestrator doesn't get completely locked out.
                    let churn_key = action_req.payload.clone();
                    let attempts = churn_tracker.entry(churn_key).or_insert(0);
                    *attempts += 1;

                    let mut constraint_texts: Vec<String> = vec![];
                    let mut rule_ids: Vec<String> = vec![];
                    let mut final_verdict: ValidateVerdict;

                    if *attempts > 3 {
                        final_verdict = ValidateVerdict::ApprovedFailOpen {
                            warning: format!(
                                "Churn limit reached ({} attempts for identical payload). Bypassing ReCognition sandbox to prevent orchestrator lockup.",
                                attempts
                            ),
                        };
                    } else {
                        // 1. Active Secret Scrubbing (Mitigates OWASP LLM06)
                        let secret_prefixes = ["sk-ant-", "sk-proj-", "ghp_", "xoxb-", "Bearer eyJ", "AKIA"];
                        let has_secrets = secret_prefixes.iter().any(|prefix| action_req.payload.contains(prefix));

                        final_verdict = if has_secrets {
                            ValidateVerdict::DeterministicReject {
                                reasons: vec!["Active Secret Scrubbing (LLM06): High-entropy secret detected in payload. Redaction Loop triggered.".to_string()],
                                constraints: vec!["Never include raw API keys, passwords, or JWTs in tool payloads. Use environment variables instead.".to_string()],
                            }
                        } else {
                            // Retrieve top constraints
                            let matched_rules = semantic_eval
                                .match_constraints(&action_req.payload)
                                .await
                                .unwrap_or(vec![]);
                            constraint_texts = matched_rules
                                .iter()
                                .map(|r| r.constraint_text.clone())
                                .collect();
                            rule_ids = matched_rules.iter().map(|r| r.id.clone()).collect();

                            let semantic_verdict = semantic_eval
                                .evaluate_intent(&action_req.payload, "Unknown Intent")
                                .await
                                .unwrap_or_else(|_| ValidateVerdict::ApprovedFailOpen {
                                    warning: "Semantic failed".to_string(),
                                });

                            // Check fast-path syntax (if the action fails semantic, we inject constraints immediately)
                            match semantic_verdict {
                                ValidateVerdict::DeterministicReject { reasons, .. } => {
                                    ValidateVerdict::DeterministicReject {
                                        reasons,
                                        constraints: constraint_texts.clone(),
                                    }
                                }
                                other => other,
                            }
                        };
                    }

                    if matches!(
                        final_verdict,
                        ValidateVerdict::SandboxPassHighFidelity { .. }
                            | ValidateVerdict::ApprovedFailOpen { .. }
                    ) {
                        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                        let sandbox_res = sandbox_eval
                            .evaluate(&action_req.action_type, &action_req.payload, &cwd)
                            .await?;

                        final_verdict = match sandbox_res {
                            ValidateVerdict::SandboxReject { reasons, logs, .. } => {
                                // Syntax Fast-Path: If error code is 2 (e.g., bash syntax error),
                                // we still reject but don't need meta-optimizer LLM logic inside the client.
                                // We just return the constraints alongside the raw logs.
                                ValidateVerdict::SandboxReject {
                                    reasons,
                                    logs,
                                    constraints: constraint_texts.clone(),
                                }
                            }
                            other => other,
                        };
                    }

                    let res = ValidateResponse {
                        trace_id: action_req.trace_id.clone(),
                        verdict: final_verdict,
                        rule_ids_triggered: rule_ids,
                    };

                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": req["id"],
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": serde_json::to_string_pretty(&res)?
                                }
                            ]
                        }
                    });

                    let out = format!("{}\n", serde_json::to_string(&response)?);
                    stdout.write_all(out.as_bytes()).await?;
                    stdout.flush().await?;
                } else if tool_name == "learn_behavioral_rule" {
                    // NEW TOOL: For NeuroPlasticity to sync rules into LanceDB
                    let args = &params["arguments"];

                    let rule = BehavioralRule {
                        id: Uuid::new_v4().to_string(),
                        version: 1,
                        rule_class: args["rule_class"].as_str().unwrap_or("general").to_string(),
                        trigger_pattern: args["trigger_pattern"]
                            .as_str()
                            .unwrap_or("*")
                            .to_string(),
                        constraint_text: args["constraint_text"].as_str().unwrap_or("").to_string(),
                        hit_count: 0,
                        status: "active".to_string(),
                    };

                    let res = match semantic_eval.add_constraint(rule).await {
                        Ok(_) => "Successfully synchronized new behavioral rule to NeuroCortex local LanceDB.".to_string(),
                        Err(e) => format!("Failed to sync rule: {}", e),
                    };

                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": req["id"],
                        "result": {
                            "content": [{"type": "text", "text": res}]
                        }
                    });

                    let out = format!("{}\n", serde_json::to_string(&response)?);
                    stdout.write_all(out.as_bytes()).await?;
                    stdout.flush().await?;
                }
            } else if method == "initialize" {
                let init_res = json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": "neurocortex-guard", "version": "0.1.0" }
                    }
                });
                let out = format!("{}\n", serde_json::to_string(&init_res)?);
                stdout.write_all(out.as_bytes()).await?;
                stdout.flush().await?;
            } else if method == "tools/list" {
                let list_res = json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {
                        "tools": [
                            {
                                "name": "local_guard_validate",
                                "description": "Validate state-mutating actions (bash, file writes) before execution.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "action_type": { "type": "string" },
                                        "payload": { "type": "string" },
                                        "orchestrator_id": { "type": "string" },
                                        "trace_id": { "type": "string" }
                                    },
                                    "required": ["action_type", "payload", "orchestrator_id", "trace_id"]
                                }
                            },
                            {
                                "name": "learn_behavioral_rule",
                                "description": "Teach NeuroCortex a new behavioral constraint for future local_guard validations.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "rule_class": { "type": "string" },
                                        "trigger_pattern": { "type": "string" },
                                        "constraint_text": { "type": "string" }
                                    },
                                    "required": ["rule_class", "trigger_pattern", "constraint_text"]
                                }
                            }
                        ]
                    }
                });
                let out = format!("{}\n", serde_json::to_string(&list_res)?);
                stdout.write_all(out.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
    }

    Ok(())
}
