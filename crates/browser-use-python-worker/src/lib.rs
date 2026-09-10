use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct PythonWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    launch: PythonWorkerLaunch,
}

#[derive(Clone, Debug)]
struct PythonWorkerLaunch {
    program: PathBuf,
    pythonpath: OsString,
    extra_env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Serialize)]
struct RunPythonRequest {
    id: String,
    session_id: String,
    cwd: String,
    artifact_dir: String,
    code: String,
    cancel_requested: bool,
    timeout_seconds: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RunPythonResponse {
    pub id: String,
    pub ok: bool,
    pub text: String,
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub outputs: Vec<Value>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    #[serde(default)]
    pub images: Vec<Value>,
    #[serde(default)]
    pub browser_events: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PythonWorkerEvent {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

impl PythonWorker {
    pub fn start() -> Result<Self> {
        Self::start_with_env(std::iter::empty::<(&str, &str)>())
    }

    pub fn start_with_env<I, K, V>(extra_env: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut paths = Vec::new();
        if let Some(path) = installed_python_path() {
            paths.push(path);
        }
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?;
        let workspace_python = repo_root.join("python");
        if workspace_python.exists() {
            paths.push(workspace_python);
        }
        if let Some(path) = std::env::var_os("PYTHONPATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let pythonpath = std::env::join_paths(paths)?;
        let extra_env = extra_env
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_os_string(), value.as_ref().to_os_string()))
            .collect::<Vec<_>>();
        Self::start_with_default_runtime(pythonpath, &extra_env)
    }

    fn start_with_default_runtime(
        pythonpath: impl AsRef<OsStr>,
        extra_env: &[(OsString, OsString)],
    ) -> Result<Self> {
        let python = std::env::var_os("BROWSER_USE_PYTHON")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let executable = if cfg!(windows) {
                    "Scripts/python.exe"
                } else {
                    "bin/python"
                };
                std::env::var_os("VIRTUAL_ENV")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .into_iter()
                    .chain(std::iter::once(
                        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv"),
                    ))
                    .map(|venv| venv.join(executable))
                    .find(|python| python.is_file())
            })
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }));
        Self::start_with_program(&python, pythonpath.as_ref(), extra_env)
    }

    pub fn start_with_pythonpath(
        python: impl AsRef<Path>,
        pythonpath: impl AsRef<OsStr>,
    ) -> Result<Self> {
        Self::start_with_program(python.as_ref(), pythonpath.as_ref(), &[])
    }

    fn start_with_program(
        program: &Path,
        pythonpath: &OsStr,
        extra_env: &[(OsString, OsString)],
    ) -> Result<Self> {
        let launch = PythonWorkerLaunch {
            program: program.to_path_buf(),
            pythonpath: pythonpath.to_os_string(),
            extra_env: extra_env.to_vec(),
        };
        let (child, stdin, stdout) = spawn_python_worker(&launch)?;
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            launch,
        })
    }

    fn restart(&mut self) -> Result<()> {
        kill_worker_child(&mut self.child);
        let (child, stdin, stdout) = spawn_python_worker(&self.launch)?;
        self.child = child;
        self.stdin = stdin;
        self.stdout = stdout;
        Ok(())
    }

    fn read_response_line(
        &mut self,
        request_id: &str,
        timeout_seconds: Option<f64>,
        deadline: Option<Instant>,
    ) -> Result<Option<String>> {
        let Some(deadline) = deadline else {
            let mut response = String::new();
            let bytes = self.stdout.read_line(&mut response)?;
            if bytes == 0 {
                bail!("python worker exited before responding");
            }
            return Ok(Some(response));
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }

        let (tx, rx) = mpsc::channel();
        let child = &mut self.child;
        let stdout = &mut self.stdout;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut response = String::new();
                let read = stdout
                    .read_line(&mut response)
                    .map(|bytes| (bytes, response));
                let _ = tx.send(read);
            });

            match rx.recv_timeout(remaining) {
                Ok(Ok((0, _))) => bail!("python worker exited before responding"),
                Ok(Ok((_, response))) => Ok(Some(response)),
                Ok(Err(err)) => Err(err.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    kill_worker_child(child);
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                    Ok(None)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("python worker reader disconnected")
                }
            }
        })
        .with_context(|| {
            format!(
                "read python worker response for {request_id} with timeout {:?}",
                timeout_seconds
            )
        })
    }

    pub fn run(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
    ) -> Result<RunPythonResponse> {
        self.run_with_events(session_id, cwd, artifact_dir, code, |_| {})
    }

    pub fn run_with_timeout(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        timeout_seconds: Option<f64>,
    ) -> Result<RunPythonResponse> {
        self.run_with_events_and_timeout(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_seconds,
            |_| {},
        )
    }

    pub fn run_with_events(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        on_event: impl FnMut(PythonWorkerEvent),
    ) -> Result<RunPythonResponse> {
        self.run_with_events_and_timeout(session_id, cwd, artifact_dir, code, None, on_event)
    }

    pub fn run_with_events_and_timeout(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        timeout_seconds: Option<f64>,
        mut on_event: impl FnMut(PythonWorkerEvent),
    ) -> Result<RunPythonResponse> {
        let request = RunPythonRequest {
            id: format!("py-{}", self.next_id),
            session_id: session_id.to_string(),
            cwd: cwd.as_ref().display().to_string(),
            artifact_dir: artifact_dir.as_ref().display().to_string(),
            code: code.to_string(),
            cancel_requested: false,
            timeout_seconds,
        };
        self.next_id += 1;
        let line = serde_json::to_string(&request)?;
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;

        let deadline = timeout_seconds.map(|seconds| {
            let seconds = seconds.max(0.0);
            let grace = (seconds * 0.1).clamp(1.0, 2.0);
            Instant::now() + Duration::from_secs_f64(seconds + grace)
        });

        loop {
            let Some(response) = self.read_response_line(&request.id, timeout_seconds, deadline)?
            else {
                self.restart()?;
                return Ok(RunPythonResponse {
                    id: request.id.clone(),
                    ok: false,
                    text: String::new(),
                    error: Some(format!(
                        "python tool timed out after {} seconds",
                        timeout_seconds.unwrap_or_default()
                    )),
                    data: Value::Null,
                    outputs: Vec::new(),
                    artifacts: Vec::new(),
                    images: Vec::new(),
                    browser_events: Vec::new(),
                });
            };
            let trimmed = response.trim();
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => {
                    if !trimmed.is_empty() {
                        on_event(PythonWorkerEvent {
                            id: request.id.clone(),
                            event: "worker.stdout".to_string(),
                            payload: serde_json::json!({ "text": trimmed }),
                        });
                    }
                    continue;
                }
            };
            if value.get("event").is_some() {
                let event: PythonWorkerEvent =
                    serde_json::from_value(value).context("parse python worker event")?;
                if event.id == request.id {
                    on_event(event);
                }
                continue;
            }
            return serde_json::from_value(value).context("parse python worker response");
        }
    }
}

fn installed_python_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let release_python = exe_dir.parent()?.join("python");
    if release_python.exists() {
        return Some(release_python);
    }
    let sibling_python = exe_dir.join("python");
    sibling_python.exists().then_some(sibling_python)
}

fn spawn_python_worker(
    launch: &PythonWorkerLaunch,
) -> Result<(Child, ChildStdin, BufReader<ChildStdout>)> {
    let mut command = Command::new(&launch.program);
    command
        .arg("-m")
        .arg("llm_browser_worker.worker")
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONPATH", &launch.pythonpath);
    command.envs(launch.extra_env.iter().cloned());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "start python worker via {} with PYTHONPATH={}",
                launch.program.display(),
                launch.pythonpath.to_string_lossy()
            )
        })?;
    let stdin = child.stdin.take().context("python worker stdin missing")?;
    let stdout = child
        .stdout
        .take()
        .context("python worker stdout missing")?;
    Ok((child, stdin, BufReader::new(stdout)))
}

fn kill_worker_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                eprintln!("timed out waiting for python worker process to exit after kill");
                break;
            }
            Err(_) => break,
        }
    }
}

impl Drop for PythonWorker {
    fn drop(&mut self) {
        kill_worker_child(&mut self.child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(unix)]
    #[test]
    fn worker_default_runtime_never_starts_package_manager() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?;
        let temp = tempfile::tempdir()?;
        let activity = temp.path().join("package-manager-started");
        let uv = temp.path().join("uv");
        std::fs::write(
            &uv,
            "#!/bin/sh\nprintf invoked > \"$PACKAGE_MANAGER_ACTIVITY\"\nexec python3 -m llm_browser_worker.worker\n",
        )?;
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755))?;
        let path = std::env::join_paths(std::iter::once(temp.path().to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))?;
        let env = [
            (OsString::from("PATH"), path),
            (
                OsString::from("PACKAGE_MANAGER_ACTIVITY"),
                activity.as_os_str().to_owned(),
            ),
            (
                OsString::from("BH_AGENT_WORKSPACE"),
                temp.path().join("workspace").into_os_string(),
            ),
        ];
        let mut worker = PythonWorker::start_with_default_runtime(repo_root.join("python"), &env)?;
        let response = worker.run(
            "local",
            temp.path(),
            temp.path().join("artifacts"),
            "result = 6 * 7",
        )?;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.data, 42);
        assert!(!activity.exists(), "worker invoked a package manager");
        Ok(())
    }

    #[test]
    fn worker_keeps_a_persistent_namespace_per_session() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let first = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "x = 41\nprint('ready')",
        )?;
        assert!(first.ok, "{first:?}");
        assert!(first.text.contains("ready"));

        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "print(x + 1)\nresult = {'value': x + 1}",
        )?;
        assert!(second.ok, "{second:?}");
        assert!(second.text.contains("42"));
        assert_eq!(second.data["value"], 42);
        Ok(())
    }

    #[test]
    fn worker_times_out_snippets_without_losing_namespace() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run_with_timeout(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "x = 7\nimport time\ntime.sleep(5)",
            Some(0.2),
        )?;
        assert!(!response.ok, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out"),
            "{response:?}"
        );

        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "print(x)\nresult = {'value': x}",
        )?;
        assert!(second.ok, "{second:?}");
        assert_eq!(second.data["value"], 7);
        Ok(())
    }

    #[test]
    fn worker_hard_times_out_threadpool_shutdown_hang_and_recovers() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;

        let started = Instant::now();
        let response = worker.run_with_timeout(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "import concurrent.futures, time\nwith concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:\n    executor.submit(time.sleep, 5).result()",
            Some(0.2),
        )?;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hard timeout should not wait for executor shutdown"
        );
        assert!(!response.ok, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out"),
            "{response:?}"
        );

        let recovered = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = {'ok': True}",
        )?;
        assert!(recovered.ok, "{recovered:?}");
        assert_eq!(recovered.data["ok"], true);
        Ok(())
    }

    #[test]
    fn worker_host_helpers_collect_outputs_artifacts_and_images() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input.txt");
        std::fs::write(&input, "hello")?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "emit_output('chunk')\ncopy_artifact('input.txt', kind='file')\nemit_browser_live_url('https://live.example')\nresult = {'ok': True}",
        )?;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.outputs[0]["text"], "chunk");
        assert_eq!(response.artifacts[0]["kind"], "file");
        assert_eq!(response.browser_events[0]["type"], "browser.live_url");
        Ok(())
    }

    #[test]
    fn worker_tolerates_non_json_stdout_lines() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let package = temp.path().join("llm_browser_worker");
        std::fs::create_dir_all(&package)?;
        std::fs::write(package.join("__init__.py"), "")?;
        std::fs::write(
            package.join("worker.py"),
            r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    print("cloud startup chatter", flush=True)
    print(json.dumps({
        "id": request["id"],
        "ok": True,
        "text": "done",
        "error": None,
        "data": None,
        "outputs": [],
        "artifacts": [],
        "images": [],
        "browser_events": [],
    }), flush=True)
"#,
        )?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", temp.path())?;
        let mut events = Vec::new();
        let response = worker.run_with_events(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = None",
            |event| events.push(event),
        )?;

        assert!(response.ok, "{response:?}");
        assert_eq!(response.text, "done");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "worker.stdout");
        assert_eq!(events[0].payload["text"], "cloud startup chatter");
        Ok(())
    }

    #[test]
    fn worker_exposes_session_metadata_and_artifact_root_helpers() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let artifact_dir = temp.path().join("artifacts");
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run(
            "s1",
            temp.path(),
            &artifact_dir,
            "result = {'root': artifact_root(), 'metadata': session_metadata()}",
        )?;
        assert!(response.ok, "{response:?}");
        let artifact_dir = artifact_dir.canonicalize()?;
        assert_eq!(response.data["root"], artifact_dir.display().to_string());
        assert_eq!(response.data["metadata"]["session_id"], "s1");
        assert_eq!(
            response.data["metadata"]["artifact_root"],
            artifact_dir.display().to_string()
        );
        Ok(())
    }

    #[test]
    fn worker_streams_host_helper_events_before_final_response() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let mut events = Vec::new();
        let response = worker.run_with_events(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "emit_output('first')\nemit_browser_state(url='https://example.com')\nresult = 'done'",
            |event| events.push(event),
        )?;
        assert!(response.ok, "{response:?}");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "output");
        assert_eq!(events[0].payload["text"], "first");
        assert_eq!(events[1].event, "browser");
        assert_eq!(events[1].payload["type"], "browser.state");
        Ok(())
    }
}
