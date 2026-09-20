from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import TextIO


class Console:
    def __init__(self, stream: TextIO = sys.stdout) -> None:
        self.stream = stream
        self.interactive = stream.isatty() and os.environ.get("TERM") != "dumb"
        self.color = self.interactive and "NO_COLOR" not in os.environ
        self._progress_active = False
        self._progress_bucket = -1
        self._progress_name: str | None = None

    def _styled(self, value: str, *codes: str) -> str:
        if not self.color:
            return value
        return f"\033[{';'.join(codes)}m{value}\033[0m"

    def _line(self, value: str = "") -> None:
        self.finish_progress()
        print(value, file=self.stream, flush=True)

    def heading(self, value: str) -> None:
        self._line(self._styled(value, "1", "36"))

    def blank(self) -> None:
        self._line()

    def step(self, current: int, total: int, value: str) -> None:
        self._line()
        label = self._styled(f"[{current}/{total}]", "1", "36")
        self._line(f"{label} {self._styled(value, '1')}")

    def item(self, current: int, total: int, value: str) -> None:
        label = self._styled(f"[{current}/{total}]", "1", "36")
        self._line(f"{label} {value}")

    def detail(self, name: str, value: str | Path) -> None:
        label = self._styled(f"{name}:", "2")
        self._line(f"  {label} {value}")

    def identifier(self, name: str, value: str) -> None:
        label = self._styled(f"{name}:", "2")
        self._line(f"  {label} {self._styled(value, '1', '36')}")

    def metric(
        self,
        name: str,
        value: str,
        comparison: str,
        *,
        favorable: bool | None,
    ) -> None:
        label = self._styled(f"{name}:", "2")
        code = "32" if favorable is True else "31" if favorable is False else "2"
        change = self._styled(comparison, code)
        self._line(f"  {label} {value} · {change}")

    def success(self, value: str) -> None:
        self._line(f"{self._styled('✓', '1', '32')} {value}")

    def activity(self, value: str) -> None:
        self._line(f"{self._styled('→', '36')} {value}")

    def warning(self, value: str) -> None:
        self._line(f"{self._styled('!', '1', '33')} {value}")

    def error(self, value: str) -> None:
        lines = value.splitlines() or ["Unknown error"]
        self._line()
        self._line(f"{self._styled('✗', '1', '31')} {lines[0]}")
        for line in lines[1:]:
            self._line(f"  {line}")

    def prompt(self, value: str) -> str:
        self.finish_progress()
        label = self._styled("?", "1", "33")
        return input(f"{label} {value}")

    def progress(self, name: str, current: int, total: int) -> None:
        if self._progress_name != name:
            self.finish_progress()
            self._progress_bucket = -1
            self._progress_name = name
        ratio = min(1.0, current / total) if total else 1.0
        current_text = f"{current:,}".rjust(len(f"{total:,}"))
        line = f"  {name:<12} {current_text} / {total:,}  {ratio:.1%}"
        if self.interactive:
            width = 24
            filled = round(ratio * width)
            bar = "━" * filled + "─" * (width - filled)
            line = (
                f"  {name:<12} {self._styled(bar, '36')}  "
                f"{ratio:.1%}  {current_text} / {total:,}"
            )
            self.stream.write(f"\r\033[2K{line}")
            self.stream.flush()
            self._progress_active = True
            return
        bucket = min(10, int(ratio * 10))
        if current not in {0, total} and bucket == self._progress_bucket:
            return
        self._progress_bucket = bucket
        print(line, file=self.stream, flush=True)

    def finish_progress(self) -> None:
        if self._progress_active:
            self.stream.write("\n")
            self.stream.flush()
            self._progress_active = False
        self._progress_name = None
