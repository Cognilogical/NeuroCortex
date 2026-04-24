use log::{info, warn};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::models::ValidateVerdict;

pub struct SandboxEvaluator {
    engine: String, // "podman" or "docker"
    available: bool,
}

impl SandboxEvaluator {
    pub async fn new() -> Self {
        let (engine, available) = Self::detect_engine().await;
        Self { engine, available }
    }

    async fn detect_engine() -> (String, bool) {
        // Prefer podman
        if let Ok(output) = Command::new("podman").arg("--version").output().await {
            if output.status.success() {
                return ("podman".to_string(), true);
            }
        }

        // Fallback to docker
        if let Ok(output) = Command::new("docker").arg("--version").output().await {
            if output.status.success() {
                return ("docker".to_string(), true);
            }
        }

        ("none".to_string(), false)
    }

    pub async fn evaluate(
        &self,
        action_type: &str,
        payload: &str,
        cwd: &str,
    ) -> anyhow::Result<ValidateVerdict> {
        if !self.available {
            return Ok(ValidateVerdict::ApprovedFailOpen{
                warning: "No container engine (Podman/Docker) available. Skipping sandbox evaluation. Please install Podman for full security.".to_string() 
            });
        }

        // We only sandbox bash or known script commands for now
        if action_type != "bash" && action_type != "script" {
            // Fast path: if it's just a file write, maybe we rely purely on deterministic AST
            // For now, fail open for unsupported action types
            return Ok(ValidateVerdict::ApprovedFailOpen {
                warning: format!(
                    "Sandbox evaluation not supported for action_type: {}",
                    action_type
                ),
            });
        }

        info!("Executing sandbox evaluation using engine: {}", self.engine);

        let start = std::time::Instant::now();

        // Command execution inside ephemeral container
        // --rm : ephemeral
        // --network=none : isolate
        // -v cwd:/workspace:ro : mount project read-only
        // --tmpfs /tmp : scratch space
        let child = Command::new(&self.engine)
            .args([
                "run",
                "--rm",
                "--network=none",
                &format!("-v={}:/workspace:ro", cwd),
                "--tmpfs=/tmp",
                "-w=/workspace",
                "alpine:latest",
                "sh",
                "-c",
                payload,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Wait with a strict timeout (e.g., 5 seconds for inline guard)
        let execution_result = timeout(Duration::from_secs(5), child.wait_with_output()).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match execution_result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();

                // Truncate logs to prevent LLM context overflow which causes death spirals and compaction errors
                if stdout.len() > 500 {
                    stdout = format!("...[TRUNCATED {} chars]...\n{}", stdout.len() - 500, &stdout[stdout.len() - 500..]);
                }
                if stderr.len() > 500 {
                    stderr = format!("...[TRUNCATED {} chars]...\n{}", stderr.len() - 500, &stderr[stderr.len() - 500..]);
                }

                if output.status.success() {
                    Ok(ValidateVerdict::SandboxPassLowFidelity {
                        notes: vec![format!("Execution succeeded in {}ms", duration_ms)],
                    })
                } else {
                    // It failed the dry-run
                    Ok(ValidateVerdict::SandboxReject {
                        reasons: vec![format!(
                            "Sandbox returned exit code: {:?}",
                            output.status.code()
                        )],
                        logs: format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr),
                        constraints: vec![], // Injected later by main
                    })
                }
            }
            Ok(Err(e)) => {
                warn!("Sandbox execution failed to run: {}", e);
                Ok(ValidateVerdict::ApprovedFailOpen {
                    warning: format!("Sandbox execution error: {}", e),
                })
            }
            Err(_) => {
                // Timeout
                warn!("Sandbox execution timed out after 5 seconds");
                // Fail open to avoid blocking the primary agent
                Ok(ValidateVerdict::ApprovedFailOpen {
                    warning: "Sandbox timed out, bypassing to prevent latency".to_string(),
                })
            }
        }
    }
}
