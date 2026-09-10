from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import shutil
import signal
import sys
import traceback
import mimetypes
import re
from pathlib import Path
from typing import Any, Dict


_namespaces: Dict[str, Dict[str, Any]] = {}
_explicit_agent_workspace = os.environ.get("BH_AGENT_WORKSPACE")


_HINT_PATTERNS = [
    (
        re.compile(r"':contains' is not a valid (CSS )?selector"),
        "':contains' is jQuery, not CSS. Use Array.from(document.querySelectorAll(sel)).filter(el => el.textContent.includes('X')).",
    ),
    (
        re.compile(r"Identifier '[^']+' has already been declared"),
        "Browser JS execution contexts persist. Wrap repeated const/let/class declarations in an IIFE like (()=>{...})() or use var.",
    ),
    (
        re.compile(r"Blocked a frame with origin .+ from accessing a cross-origin frame"),
        "Cross-origin iframe DOM access is blocked by the browser. Use iframe_target() or target-level CDP instead.",
    ),
    (
        re.compile(r"-32602.*No target with given id found"),
        "The target closed or was replaced. Call Target.getTargets/list_tabs() and use a fresh targetId.",
    ),
    (
        re.compile(r"Runtime\.getExecutionContexts.*wasn't found"),
        "Runtime.getExecutionContexts is not a CDP method. Use Target.getTargets/list_tabs(), then attach to the target you need.",
    ),
]


class _ToolTimeoutError(TimeoutError):
    pass


def _raise_tool_timeout(signum: int, frame: Any) -> None:
    raise _ToolTimeoutError("python tool timed out")


def _jsonable(value: Any) -> Any:
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)


_MISSING_STRING_VALUES = {
    "n a",
    "na",
    "nil",
    "none",
    "null",
    "not applicable",
    "tbd",
    "to be determined",
    "unknown",
}

_MISSING_STRING_PHRASES = (
    "cannot determine",
    "could not determine",
    "not accessible",
    "not available",
    "not captured",
    "not disclosed",
    "not found",
    "not listed",
    "not provided",
    "not shown",
    "not specified",
    "not visible",
    "unable to determine",
    "unavailable",
)


def _normalized_missing_text(value: str) -> str:
    text = re.sub(r"[_/\\|]+", " ", value.strip().lower())
    text = re.sub(r"[\s.,:;()[\]{}-]+", " ", text)
    return text.strip()


def _is_missing(value: Any) -> bool:
    if value is None:
        return True
    if isinstance(value, str):
        normalized = _normalized_missing_text(value)
        if not normalized:
            return True
        if normalized in _MISSING_STRING_VALUES or normalized.startswith("unknown from "):
            return True
        return any(phrase in normalized for phrase in _MISSING_STRING_PHRASES)
    if isinstance(value, (list, tuple, set, dict)):
        return len(value) == 0
    return False


def _audit_looks_computed(audit: Dict[str, Any]) -> bool:
    checks = audit.get("checks")
    return (
        audit.get("generated_by") == "audit_artifact"
        and isinstance(audit.get("record_count"), int)
        and isinstance(checks, dict)
        and "ready_for_done" in audit
    )


def _field_value(record: Any, field: str) -> Any:
    current = record
    for part in str(field).split("."):
        if isinstance(current, dict):
            current = current.get(part)
        else:
            current = getattr(current, part, None)
        if current is None:
            return None
    return current


def _dedupe_key(record: Any, fields: list[str] | None) -> str:
    if fields:
        parts = [_field_value(record, field) for field in fields]
    else:
        parts = [record]
    return json.dumps(parts, ensure_ascii=False, sort_keys=True, default=str)


def _metric_number(value: Any) -> float | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        normalized = value.replace(",", "")
        match = re.search(r"-?\d+(?:\.\d+)?", normalized)
        if match:
            return float(match.group(0))
    return None


def _records_from_path(data: Any, record_path: str | None) -> Any:
    if not record_path:
        return data
    current = [data]
    for raw_part in record_path.split("."):
        is_many = raw_part.endswith("[]")
        part = raw_part[:-2] if is_many else raw_part
        next_values = []
        for value in current:
            if part:
                if isinstance(value, dict):
                    value = value.get(part)
                else:
                    value = getattr(value, part, None)
            if is_many:
                if isinstance(value, list):
                    next_values.extend(value)
            else:
                next_values.append(value)
        current = next_values
    return current


def _load_json_or_csv(path: Path) -> Any:
    if path.suffix.lower() == ".csv":
        import csv

        with path.open(newline="", encoding="utf-8") as handle:
            return list(csv.DictReader(handle))
    return json.loads(path.read_text(encoding="utf-8"))


def _visual_file_audit(paths: list[Any]) -> list[Dict[str, Any]]:
    checks: list[Dict[str, Any]] = []
    for raw_path in paths:
        path = Path(str(raw_path)).expanduser()
        record: Dict[str, Any] = {"path": str(path), "exists": path.exists()}
        if path.exists() and path.is_file():
            record["bytes"] = path.stat().st_size
            try:
                from PIL import Image, ImageStat

                with Image.open(path) as img:
                    rgb = img.convert("RGB")
                    small = rgb.resize((min(64, rgb.width), min(64, rgb.height)))
                    colors = small.getcolors(maxcolors=1_000_000) or []
                    stat = ImageStat.Stat(rgb)
                    record.update(
                        {
                            "width": rgb.width,
                            "height": rgb.height,
                            "sample_unique_colors": len(colors),
                            "mean_rgb": [round(value, 1) for value in stat.mean],
                            "appears_blank": len(colors) <= 1,
                        }
                    )
            except Exception as exc:
                record["image_error"] = str(exc)
        checks.append(record)
    return checks


def _annotate_error(msg: str) -> str:
    for pattern, hint in _HINT_PATTERNS:
        if pattern.search(msg):
            return f"{msg}\nHint: {hint}"
    return msg


def _agent_workspace_path(cwd: Path) -> Path:
    explicit_agent_workspace = os.environ.get("BH_AGENT_WORKSPACE") or _explicit_agent_workspace
    if explicit_agent_workspace:
        return Path(explicit_agent_workspace).expanduser()
    home = Path(os.environ.get("BUT_HOME") or Path.home() / ".browser-use-terminal")
    return (home / "agent-workspace").expanduser()


def _outputs_dir_path(cwd: Path) -> Path:
    return cwd


def _emit_protocol_event(request_id: str, event: str, payload: Dict[str, Any]) -> None:
    print(
        json.dumps(
            {
                "id": request_id,
                "event": event,
                "payload": _jsonable(payload),
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ),
        file=sys.__stdout__,
        flush=True,
    )


def _record_browser_event(
    ns: Dict[str, Any],
    request_id: str,
    event_type: str,
    payload: Dict[str, Any],
) -> Dict[str, Any]:
    record = {"type": event_type, "payload": payload}
    ns.setdefault("browser_events", []).append(record)
    _emit_protocol_event(request_id, "browser", record)
    return record


def _namespace(session_id: str, cwd: Path, artifact_dir: Path) -> Dict[str, Any]:
    ns = _namespaces.setdefault(
        session_id,
        {
            "__name__": "__browser_use_worker__",
            "session_id": session_id,
        },
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)
    _outputs_dir_path(cwd).mkdir(parents=True, exist_ok=True)
    workspace = _agent_workspace_path(cwd)
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "agent_helpers.py").touch(exist_ok=True)
    ns["cwd"] = cwd
    ns["artifact_dir"] = artifact_dir
    ns["result"] = None
    ns["images"] = []
    ns["outputs"] = []
    ns["artifacts"] = []
    ns["browser_events"] = []
    return ns


def _safe_name(name: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in name).strip("._") or "artifact"


def _load_agent_helpers_into_ns(
    ns: Dict[str, Any],
    path: Any | None = None,
    force: bool = False,
) -> Dict[str, Any]:
    cwd = Path(str(ns.get("cwd") or ".")).expanduser()
    workspace = _agent_workspace_path(cwd)
    helper_path = Path(str(path)).expanduser() if path is not None else workspace / "agent_helpers.py"
    if not helper_path.is_absolute():
        helper_path = cwd / helper_path
    if not helper_path.exists():
        return {"loaded": False, "path": str(helper_path), "reason": "missing"}
    stat = helper_path.stat()
    cache_key = str(helper_path.resolve())
    stamp = (stat.st_mtime_ns, stat.st_size)
    loaded_cache = ns.setdefault("__agent_helpers_loaded__", {})
    if not force and loaded_cache.get(cache_key) == stamp:
        return {"loaded": False, "path": str(helper_path), "reason": "unchanged"}
    spec = importlib.util.spec_from_file_location(
        f"browser_use_agent_helpers_{_safe_name(str(ns.get('session_id') or 'default'))}",
        helper_path,
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load agent helper module from {helper_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    names = []
    for name, value in vars(module).items():
        if name.startswith("_"):
            continue
        ns[name] = value
        names.append(name)
    loaded_cache[cache_key] = stamp
    return {"loaded": True, "path": str(helper_path), "names": sorted(names)}


def _install_host_helpers(ns: Dict[str, Any], request_id: str, cancel_requested: bool) -> None:
    artifact_dir = Path(ns["artifact_dir"])
    def emit_output(text: Any) -> None:
        record = {"text": str(text)}
        ns.setdefault("outputs", []).append(record)
        _emit_protocol_event(request_id, "output", record)

    def _copy_artifact(
        path: Any,
        kind: str = "file",
        name: str | None = None,
        mime: str | None = None,
        emit_event: bool = True,
    ) -> Dict[str, Any]:
        source = Path(str(path)).expanduser()
        if not source.is_absolute():
            source = Path.cwd() / source
        if not source.exists() or not source.is_file():
            raise FileNotFoundError(str(source))
        target_dir = artifact_dir / ("images" if kind == "image" else "files")
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name or source.name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        shutil.copy2(source, target)
        record = {
            "kind": kind,
            "path": str(target),
            "source_path": str(source),
            "mime": mime or mimetypes.guess_type(str(target))[0],
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        if emit_event:
            _emit_protocol_event(request_id, "artifact", record)
        return record

    def copy_artifact(path: Any, kind: str = "file", name: str | None = None, mime: str | None = None) -> Dict[str, Any]:
        return _copy_artifact(path, kind=kind, name=name, mime=mime, emit_event=True)

    def _write_artifact(name: str, content: str, mime: str) -> Dict[str, Any]:
        target_dir = artifact_dir / "files"
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        target.write_text(content, encoding="utf-8")
        record = {
            "kind": "file",
            "path": str(target),
            "source_path": str(target),
            "mime": mime,
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        _emit_protocol_event(request_id, "artifact", record)
        return record

    def emit_image(path: Any, label: str | None = None, detail: str = "auto", mime_type: str | None = None) -> Dict[str, Any]:
        record = _copy_artifact(path, kind="image", mime=mime_type, emit_event=False)
        image = {
            "label": label,
            "path": record["path"],
            "mime_type": record.get("mime") or "image/png",
            "detail": detail,
        }
        ns.setdefault("images", []).append(image)
        _emit_protocol_event(request_id, "image", image)
        return image

    def _final_answer_count(data: Any) -> int | None:
        if isinstance(data, list):
            return len(data)
        if isinstance(data, dict):
            list_lengths = [len(value) for value in data.values() if isinstance(value, list)]
            if list_lengths:
                return sum(list_lengths)
        return None

    def _final_answer_preview(data: Any) -> Any:
        if isinstance(data, list):
            return data[:3]
        if isinstance(data, dict):
            preview: Dict[str, Any] = {}
            for key, value in data.items():
                preview[key] = value[:3] if isinstance(value, list) else value
            return preview
        text = str(data)
        return text[:1000] + ("..." if len(text) > 1000 else "")

    def set_final_answer(
        data: Any,
        artifact_name: str | None = None,
        mime_type: str | None = None,
        audit: Dict[str, Any] | None = None,
    ) -> Dict[str, Any]:
        """Persist the final user-facing answer for a later `done` call.

        The full answer is written to `.final_answer.json` for the host and,
        when `artifact_name` is supplied, copied as a user-visible artifact.
        The Python `result` is only a compact readiness summary, avoiding huge
        transcript prints as the source of truth.
        """
        nonlocal artifact_dir
        if isinstance(data, str):
            result_text = data
            default_mime = "text/plain"
            default_name = "final_answer.txt"
        else:
            result_text = json.dumps(data, ensure_ascii=False, indent=2, default=str)
            default_mime = "application/json"
            default_name = "final_answer.json"
        artifact = None
        if artifact_name:
            artifact = _write_artifact(artifact_name, result_text, mime_type or default_mime)
        count = _final_answer_count(data)
        ready_for_done = True
        audit_note = None
        if isinstance(audit, dict):
            if not _audit_looks_computed(audit):
                ready_for_done = False
                audit_note = (
                    "attached audit does not match audit_artifact(...) output "
                    "(expected generated_by='audit_artifact', record_count, checks, and ready_for_done)"
                )
            else:
                ready_for_done = bool(audit.get("ready_for_done"))
        summary = {
            "ready": ready_for_done,
            "count": count,
            "artifact": artifact,
            "audit": audit,
            "ready_for_done": ready_for_done,
            "preview": _final_answer_preview(data),
        }
        if audit_note:
            summary["audit_note"] = audit_note
        metadata = {
            "result": result_text,
            "summary": summary,
            "artifact": artifact,
        }
        (artifact_dir / ".final_answer.json").write_text(
            json.dumps(metadata, ensure_ascii=False, indent=2, default=str),
            encoding="utf-8",
        )
        ns["final_answer"] = data
        ns["final_answer_text"] = result_text
        ns["result"] = {"final_answer": summary}
        count_text = f" count={count}" if count is not None else ""
        artifact_text = f" artifact={artifact['path']}" if artifact else ""
        audit_text = f" audit_ready_for_done={ready_for_done}" if isinstance(audit, dict) else ""
        emit_output(f"final answer ready:{count_text}{artifact_text}{audit_text}")
        return summary

    def audit_artifact(
        data: Any | None = None,
        path: Any | None = None,
        records: list[Any] | None = None,
        record_path: str | None = None,
        required_fields: list[str] | None = None,
        dedupe_fields: list[str] | None = None,
        bucket_field: str | None = None,
        bucket_targets: Dict[str, int] | None = None,
        visual_files: list[Any] | None = None,
        unique_visual_files: bool = False,
        source_evidence: Dict[str, Any] | None = None,
        required_source_fields: list[str] | None = None,
        selection_metric_field: str | None = None,
        selection_order: str = "desc",
        selection_limit: int | None = None,
        selection_pool_records: list[Any] | None = None,
        selection_key_fields: list[str] | None = None,
        allow_empty: bool = False,
        artifact_name: str = "artifact_audit.json",
    ) -> Dict[str, Any]:
        """Compute explicit pre-final checks requested by the agent."""
        source_path = None
        if records is None:
            if data is None and path is not None:
                source = Path(str(path)).expanduser()
                if not source.is_absolute():
                    source = Path.cwd() / source
                source_path = str(source)
                data = _load_json_or_csv(source)
            extracted = _records_from_path(data, record_path)
            if isinstance(extracted, list):
                records = extracted
            elif isinstance(extracted, dict):
                if isinstance(extracted.get("records"), list):
                    records = extracted["records"]
                elif isinstance(extracted.get("items"), list):
                    records = extracted["items"]
                else:
                    records = [extracted]
            elif extracted is None:
                records = []
            else:
                records = [extracted]

        audit: Dict[str, Any] = {
            "generated_by": "audit_artifact",
            "schema_version": 1,
            "ready_for_done": True,
            "source_path": source_path or (str(path) if path is not None else None),
            "record_count": len(records),
            "checks": {},
        }

        record_level_checks_requested = bool(
            required_fields
            or dedupe_fields is not None
            or bucket_field
            or bucket_targets
            or selection_metric_field
            or selection_limit is not None
            or selection_pool_records is not None
        )
        if not records and record_level_checks_requested and not allow_empty:
            audit["checks"]["record_count"] = {
                "allow_empty": False,
                "minimum": 1,
                "violation": "zero_records",
                "note": (
                    "Record-level checks were requested, but no records were audited. "
                    "Pass allow_empty=True only after proving the source genuinely has no matching records."
                ),
            }
            audit["ready_for_done"] = False

        if required_fields:
            missing: Dict[str, Any] = {}
            for field in required_fields:
                examples = []
                count = 0
                for idx, record in enumerate(records):
                    if _is_missing(_field_value(record, field)):
                        count += 1
                        if len(examples) < 5:
                            examples.append({"index": idx, "record": record})
                missing[field] = {"count": count, "examples": examples}
            audit["checks"]["missing_fields"] = missing
            if any(item["count"] for item in missing.values()):
                audit["ready_for_done"] = False

        if dedupe_fields is not None:
            seen: Dict[str, int] = {}
            duplicate_examples = []
            for idx, record in enumerate(records):
                key = _dedupe_key(record, dedupe_fields)
                if key in seen:
                    if len(duplicate_examples) < 10:
                        duplicate_examples.append(
                            {
                                "first_index": seen[key],
                                "duplicate_index": idx,
                                "key": json.loads(key),
                                "record": record,
                            }
                        )
                else:
                    seen[key] = idx
            duplicate_count = len(records) - len(seen)
            audit["checks"]["dedupe"] = {
                "fields": dedupe_fields,
                "duplicate_count": duplicate_count,
                "unique_count": len(seen),
                "examples": duplicate_examples,
            }
            if duplicate_count:
                audit["ready_for_done"] = False

        if bucket_field:
            counts: Dict[str, int] = {}
            for record in records:
                value = _field_value(record, bucket_field)
                values = value if isinstance(value, list) else [value]
                for item in values:
                    if _is_missing(item):
                        continue
                    counts[str(item)] = counts.get(str(item), 0) + 1
            check: Dict[str, Any] = {"field": bucket_field, "counts": counts}
            if bucket_targets:
                unmet = {
                    bucket: {"count": counts.get(str(bucket), 0), "target": int(target)}
                    for bucket, target in bucket_targets.items()
                    if counts.get(str(bucket), 0) < int(target)
                }
                check["targets"] = bucket_targets
                check["unmet_targets"] = unmet
                if unmet:
                    audit["ready_for_done"] = False
            audit["checks"]["buckets"] = check

        if visual_files:
            visual = _visual_file_audit(visual_files)
            audit["checks"]["visual_files"] = visual
            if any(
                (not item.get("exists")) or item.get("appears_blank") or item.get("image_error")
                for item in visual
            ):
                audit["ready_for_done"] = False
            if unique_visual_files:
                groups: Dict[str, list[str]] = {}
                for item in visual:
                    path_value = item.get("path")
                    if not path_value or not item.get("exists") or item.get("image_error"):
                        continue
                    path_obj = Path(str(path_value)).expanduser()
                    if not path_obj.is_file():
                        continue
                    digest = hashlib.sha256(path_obj.read_bytes()).hexdigest()
                    groups.setdefault(digest, []).append(str(path_obj))
                duplicate_groups = [
                    {"sha256": digest, "paths": paths}
                    for digest, paths in groups.items()
                    if len(paths) > 1
                ]
                audit["checks"]["visual_file_uniqueness"] = {
                    "duplicate_hash_group_count": len(duplicate_groups),
                    "duplicate_hash_groups": duplicate_groups[:10],
                }
                if duplicate_groups:
                    audit["ready_for_done"] = False

        if source_evidence is not None or required_source_fields:
            evidence = source_evidence if isinstance(source_evidence, dict) else {}
            missing_source: Dict[str, Any] = {}
            for field in required_source_fields or []:
                value = _field_value(evidence, field)
                if _is_missing(value):
                    missing_source[field] = {"value": value}
            audit["checks"]["source_evidence"] = {
                "required_fields": required_source_fields or [],
                "missing_fields": missing_source,
                "evidence": evidence,
            }
            if missing_source:
                audit["ready_for_done"] = False

        if selection_metric_field:
            order = str(selection_order or "desc").strip().lower()
            descending = order not in {"asc", "ascending", "low_to_high", "lowest"}
            missing_metric = []
            selected_metrics: list[tuple[int, float, Any]] = []
            for idx, record in enumerate(records):
                metric = _metric_number(_field_value(record, selection_metric_field))
                if metric is None:
                    if len(missing_metric) < 5:
                        missing_metric.append({"index": idx, "record": record})
                else:
                    selected_metrics.append((idx, metric, record))

            order_violations = []
            for left, right in zip(selected_metrics, selected_metrics[1:]):
                left_idx, left_metric, left_record = left
                right_idx, right_metric, right_record = right
                violates = right_metric > left_metric if descending else right_metric < left_metric
                if violates and len(order_violations) < 10:
                    order_violations.append(
                        {
                            "first_index": left_idx,
                            "first_metric": left_metric,
                            "first_record": left_record,
                            "next_index": right_idx,
                            "next_metric": right_metric,
                            "next_record": right_record,
                        }
                    )

            check: Dict[str, Any] = {
                "metric_field": selection_metric_field,
                "order": "desc" if descending else "asc",
                "selected_count": len(records),
                "selection_limit": selection_limit,
                "missing_metric_count": len(records) - len(selected_metrics),
                "missing_metric_examples": missing_metric,
                "order_violation_count": len(order_violations),
                "order_violation_examples": order_violations,
            }
            if missing_metric or order_violations:
                audit["ready_for_done"] = False
            if selection_limit is not None and len(records) < int(selection_limit):
                check["under_limit"] = {"count": len(records), "target": int(selection_limit)}
                audit["ready_for_done"] = False

            if selection_pool_records is not None:
                pool_metrics: list[tuple[int, float, Any]] = []
                pool_missing = 0
                for idx, record in enumerate(selection_pool_records):
                    metric = _metric_number(_field_value(record, selection_metric_field))
                    if metric is None:
                        pool_missing += 1
                    else:
                        pool_metrics.append((idx, metric, record))
                pool_metrics.sort(key=lambda item: item[1], reverse=descending)
                desired_count = int(selection_limit or len(records))
                top_pool = pool_metrics[:desired_count]
                check["candidate_pool_count"] = len(selection_pool_records)
                check["candidate_pool_metric_count"] = len(pool_metrics)
                check["candidate_pool_missing_metric_count"] = pool_missing
                check["candidate_pool_top_preview"] = [
                    {"index": idx, "metric": metric, "record": record}
                    for idx, metric, record in top_pool[:10]
                ]

                if selection_key_fields:
                    selected_keys = {_dedupe_key(record, selection_key_fields) for _, _, record in selected_metrics}
                    top_keys = {_dedupe_key(record, selection_key_fields) for _, _, record in top_pool}
                    missing_top = [
                        {"index": idx, "metric": metric, "record": record}
                        for idx, metric, record in top_pool
                        if _dedupe_key(record, selection_key_fields) not in selected_keys
                    ][:10]
                    selected_outside_top = [
                        {"index": idx, "metric": metric, "record": record}
                        for idx, metric, record in selected_metrics
                        if _dedupe_key(record, selection_key_fields) not in top_keys
                    ][:10]
                    check["candidate_pool_key_fields"] = selection_key_fields
                    check["missing_top_candidate_count"] = max(0, len(top_keys - selected_keys))
                    check["missing_top_candidate_examples"] = missing_top
                    check["selected_outside_top_count"] = max(0, len(selected_keys - top_keys))
                    check["selected_outside_top_examples"] = selected_outside_top
                    if missing_top or selected_outside_top:
                        audit["ready_for_done"] = False

            audit["checks"]["selection"] = check

        audit_path = _outputs_dir_path(Path(str(ns.get("cwd") or "."))) / artifact_name
        audit_path.parent.mkdir(parents=True, exist_ok=True)
        audit_path.write_text(json.dumps(audit, ensure_ascii=False, indent=2, default=str), encoding="utf-8")
        audit["audit_path"] = str(audit_path)
        copy_artifact(audit_path, kind="file", name=artifact_name, mime="application/json")
        ns["last_artifact_audit"] = audit
        emit_output(
            "artifact audit ready: "
            f"ready_for_done={audit['ready_for_done']} "
            f"records={audit['record_count']} "
            f"path={audit_path}"
        )
        return audit

    def get_final_answer() -> Any:
        return ns.get("final_answer")

    def emit_browser_live_url(live_url: str) -> None:
        _record_browser_event(ns, request_id, "browser.live_url", {"live_url": str(live_url)})

    def emit_browser_state(
        url: str | None = None,
        title: str | None = None,
        status: str | None = None,
        tabs: int | None = None,
        viewport: Any | None = None,
    ) -> None:
        payload: Dict[str, Any] = {}
        if url is not None:
            payload["url"] = str(url)
        if title is not None:
            payload["title"] = str(title)
        if status is not None:
            payload["status"] = str(status)
        if tabs is not None:
            payload["tabs"] = int(tabs)
        if viewport is not None:
            payload["viewport"] = viewport
        _record_browser_event(ns, request_id, "browser.state", payload)

    def check_cancel() -> None:
        if cancel_requested:
            raise KeyboardInterrupt("cancel requested")

    def artifact_root() -> str:
        return str(artifact_dir)

    def outputs_dir() -> str:
        path = _outputs_dir_path(Path(str(ns.get("cwd") or ".")))
        path.mkdir(parents=True, exist_ok=True)
        return str(path)

    def session_metadata() -> Dict[str, Any]:
        cwd_path = Path(str(ns.get("cwd") or "."))
        return {
            "session_id": str(ns.get("session_id")),
            "cwd": str(ns.get("cwd")),
            "artifact_root": str(artifact_dir),
            "outputs_dir": str(_outputs_dir_path(cwd_path)),
            "agent_workspace": str(_agent_workspace_path(cwd_path)),
        }

    def agent_workspace(create: bool = True) -> str:
        workspace = _agent_workspace_path(Path(str(ns.get("cwd") or ".")))
        if create:
            workspace.mkdir(parents=True, exist_ok=True)
        return str(workspace)

    def load_agent_helpers(path: Any | None = None, force: bool = True) -> Dict[str, Any]:
        return _load_agent_helpers_into_ns(ns, path=path, force=force)

    ns.update(
        {
            "emit_output": emit_output,
            "copy_artifact": copy_artifact,
            "emit_image": emit_image,
            "set_final_answer": set_final_answer,
            "audit_artifact": audit_artifact,
            "get_final_answer": get_final_answer,
            "emit_browser_live_url": emit_browser_live_url,
            "emit_browser_state": emit_browser_state,
            "check_cancel": check_cancel,
            "artifact_root": artifact_root,
            "outputs_dir": outputs_dir,
            "session_metadata": session_metadata,
            "agent_workspace": agent_workspace,
            "load_agent_helpers": load_agent_helpers,
        }
    )
    _load_agent_helpers_into_ns(ns)


def _run(request: Dict[str, Any]) -> Dict[str, Any]:
    request_id = str(request.get("id") or "")
    session_id = str(request.get("session_id") or "default")
    cwd = Path(str(request.get("cwd") or ".")).expanduser().resolve()
    artifact_dir = Path(str(request.get("artifact_dir") or cwd / "artifacts")).expanduser().resolve()
    code = str(request.get("code") or "")
    cancel_requested = bool(request.get("cancel_requested"))
    timeout_seconds = float(request.get("timeout_seconds") or 0)
    stdout = io.StringIO()
    ns: Dict[str, Any] | None = None
    old_cwd = Path.cwd()
    old_alarm_handler: Any = None
    alarm_armed = False
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            ns = _namespace(session_id, cwd, artifact_dir)
            _install_host_helpers(ns, request_id, cancel_requested)
        cwd.mkdir(parents=True, exist_ok=True)
        os.chdir(cwd)
        if timeout_seconds > 0 and hasattr(signal, "SIGALRM"):
            old_alarm_handler = signal.getsignal(signal.SIGALRM)
            signal.signal(signal.SIGALRM, _raise_tool_timeout)
            signal.setitimer(signal.ITIMER_REAL, timeout_seconds)
            alarm_armed = True
        assert ns is not None
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            exec(compile(code, "<browser-use-python-worker>", "exec"), ns)
        return {
            "id": request_id,
            "ok": True,
            "text": stdout.getvalue(),
            "error": None,
            "data": _jsonable(ns.get("result")),
            "outputs": _jsonable(ns.get("outputs") or []),
            "artifacts": _jsonable(ns.get("artifacts") or []),
            "images": _jsonable(ns.get("images") or []),
            "browser_events": _jsonable(ns.get("browser_events") or []),
        }
    except BaseException as exc:
        return {
            "id": request_id,
            "ok": False,
            "text": stdout.getvalue(),
            "error": _annotate_error("".join(traceback.format_exception_only(type(exc), exc)).strip()),
            "data": None,
            "outputs": _jsonable((ns or {}).get("outputs") or []),
            "artifacts": _jsonable((ns or {}).get("artifacts") or []),
            "images": [],
            "browser_events": _jsonable((ns or {}).get("browser_events") or []),
        }
    finally:
        if alarm_armed:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old_alarm_handler)
        os.chdir(old_cwd)


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            response = _run(request)
        except BaseException as exc:
            error = _annotate_error("".join(traceback.format_exception_only(type(exc), exc)).strip())
            response = {
                "id": "",
                "ok": False,
                "text": "",
                "error": error,
                "data": None,
                "images": [],
            }
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
