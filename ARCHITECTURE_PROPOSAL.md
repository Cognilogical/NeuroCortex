# NeuroCortex & NeuroPlasticity Integration Architecture

## Objective
Integrate **NeuroCortex** (an MCP-based hallucination guard) with **NeuroPlasticity** (a self-reinforced testing framework) to create a zero-marginal-cost, self-improving Maker-Checker loop for cloud LLM orchestrators.

## Core Components

### 1. NeuroCortex (The Inline Guard)
*   **Role:** Acts as an MCP server intercepting state-mutating actions (file writes, bash commands) from a primary Cloud Orchestrator.
*   **Mechanism:** Exposes the `local_guard_validate` tool.
*   **Evaluation Layers:**
    *   *Deterministic:* AST parsing, syntax checks.
    *   *Semantic:* NLI via a small embedded local LLM (e.g., Qwen2.5-Coder) to check intent alignment and prevent hallucinated paths/APIs.

### 2. **NeuroPlasticity (The Sandbox & Optimizer)**
*   **Role:** Provides the secure dry-run environment and the Meta-Optimization feedback loop.
*   **Mechanism:** 
    *   Spins up ephemeral containers (detects and uses either `podman` or `docker`) to execute proposed bash commands or code.
    *   Catches `stderr`/`stdout` logs from failed executions.
    *   Uses its embedded local LLM to generate behavioral rules (`rules.json`) that are then vectorized and stored in a local **LanceDB** database, mirroring the architecture of NeuroStrata.

## The Self-Improving Feedback Loop
1.  **Intercept:** Orchestrator proposes an action. NeuroCortex intercepts it.
2.  **Dry-Run (Sandbox):** NeuroCortex passes the action to NeuroPlasticity to run in an ephemeral container.
    *   **Fail-Open Bypass:** If the container engine (Podman/Docker) is unavailable, still booting, or times out, the guard *fails open*. It approves the action, logs a warning, and never blocks the user's primary agent from functioning.
    *   **Security (Network & Mounts):** The ephemeral containers are strictly executed with `--network=none` to prevent hallucinated data exfiltration or SSRF attacks. Workspace directories are mounted as strictly read-only, with a separate ephemeral `/tmp` overlay for scratch writes.
    *   **Guided Setup:** If no container engine is detected on the host system at all, the tool falls back to failing open but simultaneously provides the user with an automated script or a clear set of instructions to install `podman` (the recommended rootless engine), ensuring they can activate full security when ready.
3.  **Catch & Reject:** If the action hallucinates (e.g., missing dependency, bad syntax), the sandbox fails. NeuroCortex immediately returns `Rejected` to the Orchestrator (circuit breaker max 2 retries).
4.  **Meta-Optimization (Async/Background):** The failure logs are analyzed by NeuroPlasticity's local LLM. It deduces the root cause of the hallucination.
    *   **Syntax Fast-Path:** If the failure is purely a deterministic syntax or parsing error, the Meta-Optimization LLM is bypassed completely to save compute. The raw `stderr` is simply fed back to the Orchestrator for immediate self-correction. The local LLM is reserved for semantic/logical intent drifts.
5. **Behavioral Patch (LanceDB):** A new constraint is generated and embedded into the local **LanceDB** vector store (sharing architecture with NeuroStrata).
6. **Self-Correction:** On the next Orchestrator turn (or retry), NeuroCortex intercepts the action, performs a fast semantic similarity search in LanceDB against the Orchestrator's proposed intent, and injects *only the top-K most relevant historical constraints* directly into the rejection envelope. The Orchestrator's prompt remains clean, while the guard becomes hyper-contextualized.

## Verification & Metrics
*   **Intervention Rate:** Tracking frequency of `Rejected` MCP responses.
*   **Correction Success:** Measuring if Orchestrators successfully recover on the next turn.
*   **Zero-Bleed:** Ensuring 100% of destructive hallucinations are caught within the container scratch layer (when the sandbox is available).
