import os
import sys
from pathlib import Path

import pytest

from llm_browser_worker import rust_cli


@pytest.mark.skipif(os.name == "nt", reason="uses POSIX executable fixtures")
def test_source_cli_starts_without_installing_tools(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(rust_cli, "__file__", str(tmp_path / "python/llm_browser_worker/rust_cli.py"))
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.delenv("BROWSER_USE_PYTHON", raising=False)
    (tmp_path / "Cargo.toml").touch()
    installer = tmp_path / "scripts/install-agent-ripgrep.sh"
    installer.parent.mkdir()
    installer.write_text("#!/bin/sh\nprintf invoked > install-started\n")
    installer.chmod(0o755)
    cargo = tmp_path / "cargo"
    cargo.write_text(
        '#!/bin/sh\nprintf "%s\\n" "$@" > cargo-args\n'
        'printf "%s" "$BROWSER_USE_PYTHON" > python-path\n'
    )
    cargo.chmod(0o755)

    with pytest.raises(SystemExit) as exited:
        rust_cli._exec_rust_binary("browser-use-cli", "browser-use-terminal", ["--help"])

    assert exited.value.code == 0
    assert not (tmp_path / "install-started").exists()
    assert (tmp_path / "cargo-args").read_text().splitlines() == [
        "run", "--offline", "-q", "-p", "browser-use-cli", "--", "--help",
    ]
    assert (tmp_path / "python-path").read_text() == sys.executable
