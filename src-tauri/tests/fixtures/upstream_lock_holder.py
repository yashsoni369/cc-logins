"""Minimal current-cswap/proper-lockfile lock fixture; standard library only."""

from __future__ import annotations

import os
import pathlib
import sys
import time


def acquire(path: pathlib.Path, timeout: float, staleness: float) -> bool:
    started = time.monotonic()
    path.parent.mkdir(parents=True, exist_ok=True)
    while True:
        try:
            os.mkdir(path)
            return True
        except FileExistsError:
            pass
        if time.monotonic() - started > timeout:
            return False
        try:
            age = time.time() - path.stat().st_mtime
        except FileNotFoundError:
            continue
        if age > staleness:
            try:
                os.rmdir(path)
            except OSError:
                pass
            continue
        time.sleep(0.05)


def hold(path: pathlib.Path, release: pathlib.Path, ready: pathlib.Path, staleness: float) -> int:
    if not acquire(path, 2.0, staleness):
        print("FAILED", flush=True)
        return 3
    ready.write_text("ready", encoding="utf-8")
    try:
        next_touch = time.monotonic() + 3.0
        while not release.exists():
            if time.monotonic() >= next_touch:
                os.utime(path)
                next_touch = time.monotonic() + 3.0
            time.sleep(0.02)
    finally:
        try:
            os.rmdir(path)
        except FileNotFoundError:
            pass
    return 0


def contend(path: pathlib.Path, timeout: float, staleness: float) -> int:
    if not acquire(path, timeout, staleness):
        print("TIMEOUT", flush=True)
        return 2
    print("ACQUIRED", flush=True)
    os.rmdir(path)
    return 0


if __name__ == "__main__":
    mode = sys.argv[1]
    lock_path = pathlib.Path(sys.argv[2])
    if mode == "hold":
        raise SystemExit(
            hold(
                lock_path,
                pathlib.Path(sys.argv[3]),
                pathlib.Path(sys.argv[4]),
                float(sys.argv[5]),
            )
        )
    if mode == "contend":
        raise SystemExit(contend(lock_path, float(sys.argv[3]), float(sys.argv[4])))
    raise SystemExit(f"unknown mode: {mode}")
