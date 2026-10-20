#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Regenerate THIRD_PARTY_LICENSES.md from the crates this binary actually builds.

Run from anywhere: python3 scripts/third-party-licenses.py
"""

import hashlib
import json
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGET = "x86_64-unknown-linux-gnu"
PREFIXES = ("license", "licence", "copying", "copyright", "notice", "unlicense")


def run(*argv: str) -> str:
    return subprocess.run(
        argv, cwd=ROOT, check=True, text=True, capture_output=True
    ).stdout


def main() -> None:
    tree = run(
        "cargo",
        "tree",
        "--target",
        TARGET,
        "--prefix",
        "none",
        "--no-dedupe",
        "--edges",
        "normal,build",
    )
    built = set()
    for line in tree.splitlines():
        match = re.match(r"^(\S+) v([^\s]+)$", line.strip())
        if match:
            built.add((match.group(1), match.group(2)))

    metadata = json.loads(run("cargo", "metadata", "--format-version", "1"))
    packages = {(p["name"], p["version"]): p for p in metadata["packages"]}

    rows = []
    texts: dict[str, tuple[str, list[str]]] = {}
    missing = []
    for key in sorted(built):
        if key[0] == "phonon-say":
            continue
        package = packages[key]
        directory = pathlib.Path(package["manifest_path"]).parent
        files = []
        for child in directory.iterdir():
            name = child.name.lower()
            if child.is_file() and name.startswith(PREFIXES):
                files.append(child)
        licences = directory / "LICENSES"
        if licences.is_dir():
            files.extend(sorted(f for f in licences.rglob("*") if f.is_file()))
        if not files:
            missing.append((key, package.get("license"), package.get("repository")))
        for path in sorted(files):
            try:
                text = path.read_text(errors="replace").replace("\r\n", "\n").strip()
            except OSError:
                continue
            if not text:
                continue
            digest = hashlib.sha256(text.encode()).hexdigest()[:12]
            texts.setdefault(digest, (text, []))[1].append(f"{key[0]} {key[1]}")
        rows.append((key, package))

    lines = [
        "# Third-party licenses",
        "",
        "`phonon-say` is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and",
        "`LICENSE-APACHE`). Its binary includes the third-party crates listed below.",
        "The full license texts are reproduced from each crate's distributed license",
        "files, grouped where the text is identical.",
        "",
        "This file is generated from the crates Cargo builds for "
        f"`{TARGET}` by `scripts/third-party-licenses.py`; regenerate it for other",
        "targets. Some crates are MPL-2.0: they are included unmodified, and their",
        "source is available at the repository links above.",
        "",
        "## Crates",
        "",
        "| Crate | Version | License | Repository |",
        "| --- | --- | --- | --- |",
    ]
    for (name, version), package in rows:
        license_text = (package.get("license") or "see license text").replace("|", "\\|")
        repository = package.get("repository") or ""
        lines.append(f"| {name} | {version} | {license_text} | {repository} |")

    if missing:
        lines += [
            "",
            "## Crates without a bundled license file",
            "",
            "These declare the license below in `Cargo.toml`; the corresponding text is",
            "available at the repository link.",
            "",
            "| Crate | Version | License | Repository |",
            "| --- | --- | --- | --- |",
        ]
        for (name, version), license_text, repository in missing:
            lines.append(
                f"| {name} | {version} | {license_text or 'unknown'} "
                f"| {repository or ''} |"
            )

    lines += ["", "## License texts", ""]
    ordered = sorted(texts.values(), key=lambda item: sorted(item[1])[0].lower())
    for index, (text, users) in enumerate(ordered, start=1):
        unique_users = []
        for user in users:
            if user not in unique_users:
                unique_users.append(user)
        lines += [
            f"### Text {index}",
            "",
            "Used by: " + ", ".join(f"`{user}`" for user in unique_users),
            "",
            "```text",
            text,
            "```",
            "",
        ]

    output = ROOT / "THIRD_PARTY_LICENSES.md"
    output.write_text("\n".join(lines).rstrip() + "\n")
    print(
        f"wrote {output.relative_to(ROOT)}: {len(rows)} crates, "
        f"{len(ordered)} distinct license texts, {len(missing)} without a file"
    )


if __name__ == "__main__":
    main()
