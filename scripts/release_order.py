#!/usr/bin/env python3
"""Work out which crates go to crates.io, in which order, and refuse if anything is wrong.

Publishing a Cargo workspace is not a loop you can write by hand: crates.io rejects a crate
whose dependency is not there yet, so the order has to be a topological sort of the internal
dependency graph. Two things have to hold before any of it can run:

  every internal dependency carries a `version`
      `cargo publish` will not package `{ path = "../core" }` — the path does not exist on
      anyone else's machine and crates.io cannot resolve it.

  every crate is actually publishable
      `[workspace.package] publish = false` is the default here, so each member opts in with
      `publish.workspace = true`. A crate that does not opt in would fail late, after the
      crates beneath it had already gone out.

Prints one crate name per line, dependencies first. Exits non-zero on a problem, having
listed all of them rather than the first one — fixing them one commit at a time is worse than
fixing them together.

    python3 scripts/release_order.py           # print the order
    python3 scripts/release_order.py --check   # validate only, no output
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"

DEP_RE = re.compile(r"^\s*([a-z0-9-]+)\s*=\s*\{([^}]*)\}\s*$", re.M)
PATH_RE = re.compile(r"(^|,)\s*path\s*=")
VERSION_RE = re.compile(r"(^|,)\s*version(\.workspace)?\s*=")


def field(body: str, key: str) -> str | None:
    m = re.search(rf"(^|,)\s*{re.escape(key)}\s*=\s*\"([^\"]*)\"", body)
    return m.group(2) if m else None


def main() -> int:
    check_only = "--check" in sys.argv

    root_manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    ws_block = re.search(r"\[workspace\.package\]([\s\S]*?)(?=\n\[|\Z)", root_manifest)
    ws_default_publish = False
    if ws_block:
        m = re.search(r"^\s*publish\s*=\s*(true|false)", ws_block.group(1), re.M)
        ws_default_publish = bool(m and m.group(1) == "true")

    crates: dict[str, dict] = {}
    for manifest in sorted(CRATES.glob("*/Cargo.toml")):
        text = manifest.read_text(encoding="utf-8")
        name = re.search(r'^\s*name\s*=\s*"([^"]+)"', text, re.M)
        if not name:
            continue
        name = name.group(1)

        # `publish.workspace = true` overrides a workspace default of false; `publish = false`
        # in the member overrides a default of true.
        if re.search(r"^\s*publish\s*=\s*false", text, re.M):
            publishable = False
        elif re.search(r"^\s*publish\s*=\s*true", text, re.M):
            publishable = True
        elif re.search(r"^\s*publish\.workspace\s*=\s*true", text, re.M):
            publishable = True
        else:
            publishable = ws_default_publish

        deps, unversioned = {}, []
        for dep, body in DEP_RE.findall(text):
            if not PATH_RE.search(body):
                continue
            if not VERSION_RE.search(body):
                unversioned.append(dep)
            target = field(body, "path")
            # `crates/llm` carries a dev-dependency on itself (`path = "."`) to reach its
            # test-support feature. That is a self-edge, not a cycle, and it still needs a
            # version — Cargo checks dev-dependencies when packaging too.
            if target:
                resolved = (manifest.parent / target).resolve()
                if resolved != manifest.parent.resolve():
                    deps[dep] = resolved

        crates[name] = {"path": manifest.parent, "publishable": publishable, "deps": deps, "unversioned": unversioned}

    problems: list[str] = []

    for name, c in crates.items():
        for dep in c["unversioned"]:
            problems.append(f"{name}: dependency `{dep}` has a path but no version")

    published = {n for n, c in crates.items() if c["publishable"]}
    skipped = sorted(set(crates) - published)

    # A published crate depending on a path-only one that is never published is unsolvable on
    # crates.io: the dependency simply will not be there. This is separate from the missing
    # `version` above — adding a version to an unpublishable crate does not make it exist.
    for name in sorted(published):
        by_dir = {c["path"]: n for n, c in crates.items()}
        for dep, target in crates[name]["deps"].items():
            dep_name = by_dir.get(target)
            if dep_name and dep_name not in published:
                problems.append(
                    f"{name}: depends on `{dep}`, which is not published — "
                    "either publish it or stop depending on it by path"
                )

    # Topological sort over the internal edges only.
    order: list[str] = []
    state: dict[str, int] = {}
    by_dir = {c["path"]: n for n, c in crates.items()}

    def visit(name: str) -> None:
        if state.get(name) == 2:
            return
        if state.get(name) == 1:
            problems.append(f"dependency cycle through {name}")
            return
        state[name] = 1
        for target in sorted(crates[name]["deps"].values()):
            dep_name = by_dir.get(target)
            if dep_name and dep_name in published:
                visit(dep_name)
        state[name] = 2
        if name in published:
            order.append(name)

    for name in sorted(published):
        visit(name)

    if problems:
        print("crates.io cannot be published yet:", file=sys.stderr)
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        print(
            "\nThe fix for a missing version is one line per dependency; the private\n"
            "repository's sync-dep-versions.mjs writes them all:\n"
            "  node ../scripts/release/sync-dep-versions.mjs",
            file=sys.stderr,
        )
        return 1

    if skipped:
        print(f"not published: {' '.join(skipped)}", file=sys.stderr)

    if not check_only:
        print("\n".join(order))
    return 0


if __name__ == "__main__":
    sys.exit(main())