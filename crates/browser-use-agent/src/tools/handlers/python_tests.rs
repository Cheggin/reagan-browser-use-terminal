//! Tests for the python tool handler ([`PythonTool`]).
//!
//! These NEVER touch the real `browser-use-python-worker` runtime (which spawns
//! an external Python/Bun process). Instead a [`FakeBackend`] records the code
//! it receives and returns canned `RunPythonResponse` values, so the adapter's
//! mapping and routing logic can be verified in isolation. No Python, no Bun, no
//! network.
//!
//! Tests cover: (1) a code request routes to the backend and maps
//! text/outputs -> ExecOutput; (2) a backend error -> ToolError; (3)
//! parallel_safe = false; (4) empty code -> Rejected; (5) an orchestrator-driven
//! run with the fake backend.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use browser_use_python_worker::RunPythonResponse;
use serde_json::{json, Value};

use super::python::{PythonBackend, PythonRequest, PythonTool};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// A configurable fake backend. By default every call returns `canned`; set
/// `fail` to make every call return an `anyhow` error.
struct FakeBackend {
    last_code: Mutex<Option<String>>,
    last_paths: Mutex<Option<(PathBuf, PathBuf)>>,
    canned: RunPythonResponse,
    fail: bool,
}

impl FakeBackend {
    fn new(canned: RunPythonResponse) -> Self {
        Self {
            last_code: Mutex::new(None),
            last_paths: Mutex::new(None),
            canned,
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            last_code: Mutex::new(None),
            last_paths: Mutex::new(None),
            canned: ok_response("", true),
            fail: true,
        }
    }

    fn last_code(&self) -> Option<String> {
        self.last_code.lock().unwrap().clone()
    }

    fn last_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.last_paths.lock().unwrap().clone()
    }
}

impl PythonBackend for FakeBackend {
    fn run(
        &self,
        _session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: Option<f64>,
    ) -> anyhow::Result<RunPythonResponse> {
        *self.last_code.lock().unwrap() = Some(code.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        if self.fail {
            anyhow::bail!("worker exploded");
        }
        Ok(self.canned.clone())
    }
}

/// Build a canned worker response with the given text and ok flag.
fn ok_response(text: &str, ok: bool) -> RunPythonResponse {
    RunPythonResponse {
        id: "py-1".to_string(),
        ok,
        text: text.to_string(),
        error: None,
        data: Value::Null,
        outputs: Vec::new(),
        artifacts: Vec::new(),
        images: Vec::new(),
        browser_events: Vec::new(),
    }
}

fn tool_with(backend: Arc<FakeBackend>) -> PythonTool {
    PythonTool::with_backend(backend)
}

fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

fn ctx() -> ToolCtx {
    let cwd = std::env::temp_dir();
    ToolCtx {
        call_id: "call-python".to_string(),
        tool_name: "python".to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

/// Run a request directly through the runtime with a `SandboxType::None`
/// attempt (no orchestrator).
async fn run_direct(tool: &PythonTool, req: &PythonRequest) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, &ctx()).await
}

async fn run_direct_with_ctx(
    tool: &PythonTool,
    req: &PythonRequest,
    ctx: &ToolCtx,
) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

// (1) A code request routes to the backend and maps stdout/outputs -> ExecOutput.
#[tokio::test]
async fn code_routes_and_maps_output() {
    let mut canned = ok_response("hello", true);
    canned.outputs = vec![json!({ "text": "42" })];
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("print('hello'); 6 * 7");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last_code().as_deref(),
        Some("print('hello'); 6 * 7")
    );
    // text plus the appended expression output.
    assert_eq!(out.stdout, "hello\n42");
    assert_eq!(out.stderr, "");
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn default_artifact_dir_comes_from_tool_ctx_artifact_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("cwd");
    let artifact_root = root.path().join("artifacts").join("session-1");
    let backend = Arc::new(FakeBackend::new(ok_response("ok", true)));
    let tool = tool_with(Arc::clone(&backend));
    let ctx = ToolCtx {
        call_id: "call-python".to_string(),
        tool_name: "python".to_string(),
        cwd: cwd.clone(),
        artifact_root: artifact_root.clone(),
    };

    let req = PythonRequest::new("result = 1");
    run_direct_with_ctx(&tool, &req, &ctx).await.unwrap();

    assert_eq!(
        backend.last_paths(),
        Some((cwd, artifact_root)),
        "python backend should receive separate cwd and artifact root"
    );
}

// A snippet that raised maps `error` onto stderr and a non-zero exit code.
#[tokio::test]
async fn error_response_maps_to_stderr_and_nonzero_exit() {
    let mut canned = ok_response("partial", false);
    canned.error = Some("ValueError: nope".to_string());
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("raise ValueError('nope')");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.stdout, "partial");
    assert_eq!(out.stderr, "ValueError: nope");
    assert_eq!(out.exit_code, 1);
}

// (2) An error from the backend -> ToolError::Other.
#[tokio::test]
async fn backend_error_maps_to_tool_error() {
    let backend = Arc::new(FakeBackend::failing());
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("boom()");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
    // Backend was still invoked with the code before erroring.
    assert_eq!(backend.last_code().as_deref(), Some("boom()"));
}

// (3) parallel_safe = false, and approval/sandbox accessors are sane.
#[test]
fn python_is_not_parallel_safe() {
    let tool = tool_with(Arc::new(FakeBackend::new(ok_response("", true))));
    let req = PythonRequest::new("1 + 1");
    assert!(!tool.parallel_safe(&req));
    assert_eq!(tool.approval_keys(&req).len(), 1);
    assert!(tool.exec_approval_requirement(&req).is_none());
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
}

// (4) Empty code is rejected before touching the backend.
#[tokio::test]
async fn empty_code_rejected_without_calling_backend() {
    let backend = Arc::new(FakeBackend::new(ok_response("", true)));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("   \n  ");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last_code(), None);
}

// Produced artifacts and images are surfaced to the MODEL as a structured stdout
// manifest (path / kind / mime / size), not a bare count, so the model can act
// on the files the snippet created.
#[tokio::test]
async fn artifacts_and_images_are_surfaced_as_a_stdout_manifest() {
    let mut canned = ok_response("done", true);
    canned.artifacts = vec![json!({
        "kind": "file",
        "path": "out/report.csv",
        "mime": "text/csv",
        "bytes": 2048
    })];
    canned.images = vec![json!({
        "path": "out/plot.png",
        "mime_type": "image/png",
        "bytes": 9000
    })];
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("copy_artifact('out/report.csv')");
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 0);
    // Manifest is on stdout (the model-facing result), with concrete details.
    assert!(
        out.stdout.contains("[python:artifacts (1)]"),
        "{:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("file: out/report.csv"),
        "{:?}",
        out.stdout
    );
    assert!(out.stdout.contains("text/csv"), "{:?}", out.stdout);
    assert!(out.stdout.contains("2048 bytes"), "{:?}", out.stdout);
    assert!(
        out.stdout.contains("[python:images (1)]"),
        "{:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("image: out/plot.png"),
        "{:?}",
        out.stdout
    );
    assert!(out.stdout.contains("image/png"), "{:?}", out.stdout);
    // The original text is preserved ahead of the manifest.
    assert!(out.stdout.starts_with("done"), "{:?}", out.stdout);
}

// A structured `result = ...` value (the worker's `data`) is rendered into the
// model-facing stdout so the model sees the actual value, not just a count.
#[tokio::test]
async fn structured_result_is_rendered_into_stdout() {
    let mut canned = ok_response("", true);
    canned.data = json!({ "answer": 42, "items": ["a", "b"] });
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("result = {'answer': 42, 'items': ['a','b']}");
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("[python:result]"), "{:?}", out.stdout);
    assert!(out.stdout.contains("\"answer\": 42"), "{:?}", out.stdout);
    assert!(out.stdout.contains("\"items\""), "{:?}", out.stdout);
}

#[tokio::test]
async fn traceback_is_preserved_with_partial_output() {
    let mut canned = ok_response("partial output", false);
    canned.error = Some("Traceback (most recent call last):\n  ValueError: nope".to_string());
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("raise ValueError('nope')");
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 1);
    // The pre-failure stdout is preserved.
    assert!(out.stdout.contains("partial output"), "{:?}", out.stdout);
    // The traceback is on stderr verbatim.
    assert!(out.stderr.contains("Traceback"), "{:?}", out.stderr);
    assert!(out.stderr.contains("ValueError: nope"), "{:?}", out.stderr);
}

// Oversized stdout is capped (the full text is persisted durably elsewhere) so a
// runaway snippet cannot flood the model context.
#[tokio::test]
async fn oversized_stdout_is_capped() {
    use super::python::MAX_INLINE_STDOUT_BYTES;
    let big = "x".repeat(MAX_INLINE_STDOUT_BYTES + 5_000);
    let canned = ok_response(&big, true);
    let backend = Arc::new(FakeBackend::new(canned));
    let tool = tool_with(Arc::clone(&backend));

    let req = PythonRequest::new("print('x' * 99999)");
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.len() < big.len(), "should be capped");
    assert!(
        out.stdout.contains("[stdout truncated"),
        "{:?}",
        &out.stdout[..200]
    );
}

// (5) Orchestrator-driven run with the fake backend (no Python/Bun).
#[tokio::test]
async fn orchestrated_python_runs_under_none() {
    let backend = Arc::new(FakeBackend::new(ok_response("from orchestrator", true)));
    let tool = tool_with(Arc::clone(&backend));
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let env = TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    };

    let req = PythonRequest::new("print('from orchestrator')");
    let result = orch
        .run(&tool, &req, &ctx(), &env, AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.contains("from orchestrator"));
    assert_eq!(
        backend.last_code().as_deref(),
        Some("print('from orchestrator')")
    );
}
