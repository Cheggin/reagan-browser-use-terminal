import json
import os
import subprocess
import sys
from pathlib import Path

from llm_browser_worker import worker


def test_worker_starts_without_browser_imports_or_background_network(tmp_path: Path) -> None:
    activity = tmp_path / "activity"
    (tmp_path / "sitecustomize.py").write_text(
        "import pathlib, sys\n"
        "def audit(event, args):\n"
        "    if event in {'socket.connect', 'socket.getaddrinfo', 'subprocess.Popen', 'os.system'}:\n"
        f"        pathlib.Path({str(activity)!r}).write_text(event)\n"
        "        raise RuntimeError('unexpected background activity')\n"
        "sys.addaudithook(audit)\n"
    )
    harness = tmp_path / "browser_harness"
    harness.mkdir()
    (harness / "__init__.py").write_text(
        f"from pathlib import Path\nPath({str(activity)!r}).write_text('browser import')\n"
    )
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join([str(tmp_path), str(Path(worker.__file__).parents[1])])
    env["BH_AGENT_WORKSPACE"] = str(tmp_path / "workspace")
    request = {
        "id": "local",
        "session_id": "local",
        "cwd": str(tmp_path),
        "artifact_dir": str(tmp_path / "artifacts"),
        "code": "result = 6 * 7",
    }
    completed = subprocess.run(
        [sys.executable, "-m", "llm_browser_worker.worker"],
        input=json.dumps(request) + "\n",
        text=True,
        capture_output=True,
        check=True,
        env=env,
    )
    response = json.loads(completed.stdout)
    assert response["ok"] is True, response
    assert response["data"] == 42
    assert not activity.exists(), activity.read_text()


def test_worker_run_executes_in_persistent_session_namespace(tmp_path: Path) -> None:
    first = worker._run(
        {
            "id": "one",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter\nemit_output(f'counter={counter}')",
        }
    )
    second = worker._run(
        {
            "id": "two",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter",
        }
    )

    assert first["ok"] is True
    assert first["data"] == 1
    assert first["outputs"] == [{"text": "counter=1"}]
    assert second["ok"] is True
    assert second["data"] == 2


def test_worker_records_artifacts_and_images(tmp_path: Path) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"png")

    response = worker._run(
        {
            "id": "image",
            "session_id": "task-2",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": f"emit_image({str(source)!r}, label='shot', mime_type='image/png')",
        }
    )

    assert response["ok"] is True
    assert response["images"][0]["label"] == "shot"
    assert response["images"][0]["mime_type"] == "image/png"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_records_browser_state_details(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "browser-state",
            "session_id": "task-3",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "emit_browser_state(url='https://example.com', title='Example', status='connected', tabs=2, viewport={'w': 1440, 'h': 900})",
        }
    )

    assert response["ok"] is True
    assert response["browser_events"] == [
        {
            "type": "browser.state",
            "payload": {
                "url": "https://example.com",
                "title": "Example",
                "status": "connected",
                "tabs": 2,
                "viewport": {"w": 1440, "h": 900},
            },
        }
    ]


def test_worker_autoloads_agent_workspace_helpers(tmp_path: Path, monkeypatch) -> None:
    workspace = tmp_path / ".browser-use-terminal" / "agent-workspace"
    monkeypatch.setenv("BH_AGENT_WORKSPACE", str(workspace))
    workspace.mkdir(parents=True)
    (workspace / "agent_helpers.py").write_text(
        "def helper_value():\n    return 42\n",
        encoding="utf-8",
    )

    response = worker._run(
        {
            "id": "agent-helpers",
            "session_id": "task-agent-helpers",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = {'workspace': agent_workspace(create=False), 'value': helper_value()}",
        }
    )

    assert response["ok"] is True
    assert response["data"]["workspace"] == str(workspace)
    assert response["data"]["value"] == 42


def test_worker_error_hints_are_appended(tmp_path: Path) -> None:
    cases = [
        (
            "raise RuntimeError(\"':contains' is not a valid CSS selector\")",
            "':contains' is jQuery, not CSS.",
        ),
        (
            "raise RuntimeError(\"Identifier 'buttons' has already been declared\")",
            "execution contexts persist",
        ),
        (
            "raise RuntimeError('Blocked a frame with origin https://a from accessing a cross-origin frame')",
            "Cross-origin iframe DOM access",
        ),
        (
            "raise RuntimeError('-32602 No target with given id found')",
            "target closed or was replaced",
        ),
        (
            "raise RuntimeError(\"Runtime.getExecutionContexts wasn't found\")",
            "Runtime.getExecutionContexts is not a CDP method",
        ),
    ]

    for idx, (code, expected_hint) in enumerate(cases):
        response = worker._run(
            {
                "id": f"hint-{idx}",
                "session_id": f"task-hint-{idx}",
                "cwd": str(tmp_path),
                "artifact_dir": str(tmp_path / "artifacts"),
                "code": code,
            }
        )
        assert response["ok"] is False
        assert "Hint:" in response["error"]
        assert expected_hint in response["error"]


def test_worker_set_final_answer_persists_metadata_and_compact_result(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "final-answer",
            "session_id": "task-final-answer",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "summary = set_final_answer({'stores': [{'name': 'A', 'address': 'B'}]}, artifact_name='stores.json')\nresult = summary",
        }
    )

    assert response["ok"] is True
    assert response["data"]["count"] == 1
    assert response["outputs"][0]["text"].startswith("final answer ready:")
    assert Path(response["data"]["artifact"]["path"]).exists()
    metadata = tmp_path / "artifacts" / ".final_answer.json"
    assert metadata.exists()
    assert '"stores"' in metadata.read_text()


def test_worker_audit_artifact_reports_general_quality_checks(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "artifact-audit",
            "session_id": "task-artifact-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": """
rows = [
    {'name': 'A', 'category': 'one', 'score': 10},
    {'name': '', 'category': 'one', 'score': 7},
    {'name': 'A', 'category': 'two', 'score': 3},
]
audit = audit_artifact(
    records=rows,
    required_fields=['name', 'category'],
    dedupe_fields=['name'],
    bucket_field='category',
    bucket_targets={'one': 3, 'two': 1},
)
result = audit
""",
        }
    )

    assert response["ok"] is True
    audit = response["data"]
    assert audit["ready_for_done"] is False
    assert audit["generated_by"] == "audit_artifact"
    assert audit["record_count"] == 3
    assert audit["checks"]["missing_fields"]["name"]["count"] == 1
    assert audit["checks"]["dedupe"]["duplicate_count"] == 1
    assert audit["checks"]["buckets"]["unmet_targets"] == {
        "one": {"count": 2, "target": 3}
    }
    assert Path(audit["audit_path"]).exists()
    assert response["artifacts"][0]["source_path"] == audit["audit_path"]


def test_worker_audit_artifact_zero_records_requires_explicit_empty_proof(
    tmp_path: Path,
) -> None:
    blocked = worker._run(
        {
            "id": "artifact-zero-record-audit",
            "session_id": "task-artifact-zero-record-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "audit = audit_artifact(records=[], required_fields=['name'])\nresult = audit",
        }
    )
    assert blocked["data"]["ready_for_done"] is False
    assert blocked["data"]["checks"]["record_count"]["violation"] == "zero_records"

    allowed = worker._run(
        {
            "id": "artifact-zero-record-audit-allowed",
            "session_id": "task-artifact-zero-record-audit-allowed",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts2"),
            "code": "audit = audit_artifact(records=[], required_fields=['name'], allow_empty=True)\nresult = audit",
        }
    )
    assert allowed["data"]["ready_for_done"] is True


def test_worker_set_final_answer_embeds_explicit_audit(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "final-answer-audit",
            "session_id": "task-final-answer-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "rows=[{'name':''}]\naudit=audit_artifact(records=rows, required_fields=['name'])\nsummary=set_final_answer({'rows': rows}, artifact_name='rows.json', audit=audit)\nresult=summary",
        }
    )

    assert response["ok"] is True
    assert response["data"]["ready_for_done"] is False
    assert response["data"]["audit"]["checks"]["missing_fields"]["name"]["count"] == 1
    assert "audit_ready_for_done=False" in response["outputs"][-1]["text"]


def test_worker_audit_artifact_reports_selection_metric_gaps(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "artifact-selection-audit",
            "session_id": "task-artifact-selection-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": """
selected = [{'id': 'b', 'score': 7}, {'id': 'a', 'score': 10}]
pool = [{'id': 'a', 'score': 10}, {'id': 'b', 'score': 7}, {'id': 'c', 'score': 11}]
audit = audit_artifact(
    records=selected,
    selection_metric_field='score',
    selection_order='desc',
    selection_limit=2,
    selection_pool_records=pool,
    selection_key_fields=['id'],
)
result = audit
""",
        }
    )

    audit = response["data"]
    assert audit["ready_for_done"] is False
    selection = audit["checks"]["selection"]
    assert selection["order_violation_count"] == 1
    assert selection["missing_top_candidate_count"] == 1
    assert selection["selected_outside_top_count"] == 1
