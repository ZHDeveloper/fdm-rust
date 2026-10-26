#!/usr/bin/env python3
"""Copy the transitive DLL closure of the given PE executables.

Usage: amule_dll_closure.py <bin_dir> <out_dir> <exe> [<exe> ...]
"""
import os
import shutil
import sys

try:
    import pefile
except ImportError:  # fallback: copy every file from bin/ (larger, but still functional)
    pefile = None


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    bin_dir = os.path.abspath(sys.argv[1])
    out_dir = os.path.abspath(sys.argv[2])
    exes = sys.argv[3:]
    os.makedirs(out_dir, exist_ok=True)

    available = {}
    for name in os.listdir(bin_dir):
        available[name.lower()] = os.path.join(bin_dir, name)

    pending = []
    for exe in exes:
        src = exe if os.path.isabs(exe) else os.path.join(bin_dir, exe)
        if not os.path.isfile(src):
            print(f"missing executable: {src}", file=sys.stderr)
            return 1
        pending.append(src)

    if pefile is None:
        copied = 0
        for name in os.listdir(bin_dir):
            src = os.path.join(bin_dir, name)
            if os.path.isfile(src):
                shutil.copy2(src, os.path.join(out_dir, name))
                copied += 1
        total = sum(os.path.getsize(os.path.join(out_dir, n)) for n in os.listdir(out_dir))
        print(f"pefile unavailable; copied {copied} files, {total / (1024 * 1024):.1f} MiB")
        return 0

    copied = set()
    while pending:
        src = pending.pop()
        key = os.path.basename(src).lower()
        if key in copied:
            continue
        copied.add(key)
        shutil.copy2(src, os.path.join(out_dir, os.path.basename(src)))
        try:
            pe = pefile.PE(src, fast_load=True)
            pe.parse_data_directories(
                directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"]]
            )
            imports = getattr(pe, "DIRECTORY_ENTRY_IMPORT", []) or []
            for entry in imports:
                dll = entry.dll.decode("utf-8", "replace").lower()
                if dll in copied:
                    continue
                candidate = available.get(dll)
                if candidate:
                    pending.append(candidate)
            pe.close()
        except Exception as exc:  # noqa: BLE001 - best-effort packaging helper
            print(f"warning: cannot inspect {src}: {exc}", file=sys.stderr)

    total = sum(os.path.getsize(os.path.join(out_dir, n)) for n in os.listdir(out_dir))
    print(f"copied {len(copied)} files, {total / (1024 * 1024):.1f} MiB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
