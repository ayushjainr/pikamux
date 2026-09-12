"""Run the frozen Python reference for comparative, side-effect-free benchmarks.

Apple's system Python 3.9 predates ``dataclass(slots=True)``. Removing that one
memory-layout keyword does not change the CLI startup contract being measured.
The frozen reference tree remains byte-for-byte unchanged.
"""

from __future__ import annotations

import dataclasses
import importlib.util
import runpy
import sys
import types
from pathlib import Path
from typing import Any


if sys.version_info < (3, 10):
    _dataclass = dataclasses.dataclass

    def _compatible_dataclass(_cls: Any = None, **kwargs: Any) -> Any:
        kwargs.pop("slots", None)
        if _cls is None:
            return lambda cls: _dataclass(cls, **kwargs)
        return _dataclass(_cls, **kwargs)

    dataclasses.dataclass = _compatible_dataclass

if sys.platform == "darwin" and importlib.util.find_spec("psutil") is None:
    psutil = types.ModuleType("psutil")
    psutil.Error = RuntimeError
    psutil.Process = lambda _pid: (_ for _ in ()).throw(RuntimeError("disabled"))
    psutil.pids = lambda: []
    for _status in (
        "RUNNING",
        "SLEEPING",
        "DISK_SLEEP",
        "STOPPED",
        "TRACING_STOP",
        "ZOMBIE",
        "DEAD",
        "IDLE",
    ):
        setattr(psutil, f"STATUS_{_status}", _status)
    sys.modules["psutil"] = psutil


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "reference" / "python" / "src"))
runpy.run_module("pikamux", run_name="__main__")
