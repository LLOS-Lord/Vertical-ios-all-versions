#!/usr/bin/env python3
"""Build and validate the compact `sim-base-26.3.zip` simulation runtime pack for Horizon iOS.

Runs on GitHub Actions (`ubuntu-latest` with Temurin JDK 25 + Rust toolchain):
1. Reads `research-lock.json` (`version = "26.3"`, pinned `manifest_sha1`, `client_sha1`, `server_sha1`, `java_major`).
2. Fetches the pinned `26.3.json`, `client.jar`, `server.jar`, Linux/common Maven libraries, and all
   non-`.ogg` asset-index objects (`fonts`, `panorama`, `packs`, `languages`, and `minecraft/sounds.json`
   event table) directly from Mojang's CDN, verifying every single file against its SHA-1 checksum.
3. Invokes `horizon-prepare --prepare` to run all 19 Java-25 simulation/datagen exporters and assemble
   the prepared `26.3` resource directory (`SCHEMA = 7`).
4. Installs `assets/minecraft/sounds.json` (JSON event metadata only — zero `.ogg` audio waveforms are
   bundled in `sim-base-26.3.zip`; all audio & version textures stream directly from Mojang CDN on device),
   validates every required engine table, and packs a symlink-free `sim-base-26.3.zip` + `.sha256`.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import urllib.request
import zipfile

REQUIRED_TOP_FILES = [
    "horizon-resources.json",
    "facts.json",
    "block-tags.json",
    "movement-properties.nbt",
    "attributes.json",
    "landing-definitions.json",
    "damage-types.json",
    "damage-tags.json",
    "damage-catalog.json",
    "player-float-ids.json",
    "cached-collision.nbt",
    "render-definitions.json",
    "occlusion.nbt",
    "gameplay-facts.json",
    "entity-types.json",
    "block-behaviours.json",
    "datafix-tables.json",
    "model-states.json",
    "entity-models.json",
    "mob-data.json",
    "command-registries.json",
    "registries.json",
    "commands.json",
    "items.json",
    "creative-inventory.json",
]

REQUIRED_NESTED_FILES = [
    "worldgen/blocks/block-properties.nbt",
    "worldgen/blocks/cached-collision.nbt",
    "worldgen/blocks/cached-support.nbt",
    "assets/pack.mcmeta",
    "assets/minecraft/sounds.json",
    "assets/minecraft/font/default.json",
    "assets/minecraft/lang/en_us.json",
]


def sha1_file(path: Path) -> str:
    h = hashlib.sha1()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def download_verified(url: str, dest: Path, expected_sha1: str, retries: int = 4) -> None:
    if dest.is_file() and sha1_file(dest) == expected_sha1.lower():
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    last_err: Exception | None = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "Horizon-CI-Prepare/1.0"})
            with urllib.request.urlopen(req, timeout=60) as resp, tmp.open("wb") as out:
                h = hashlib.sha1()
                while True:
                    buf = resp.read(256 * 1024)
                    if not buf:
                        break
                    h.update(buf)
                    out.write(buf)
            got = h.hexdigest()
            if got != expected_sha1.lower():
                tmp.unlink(missing_ok=True)
                raise RuntimeError(f"SHA-1 mismatch for {url}: expected {expected_sha1}, got {got}")
            tmp.replace(dest)
            return
        except Exception as exc:  # noqa: BLE001
            last_err = exc
            tmp.unlink(missing_ok=True)
            time.sleep(0.5 * (attempt + 1))
    raise RuntimeError(f"Failed downloading {url}: {last_err}")


def rule_applies_on_linux(lib: dict) -> bool:
    rules = lib.get("rules")
    if not rules:
        return True
    allowed = False
    for rule in rules:
        action = rule.get("action") == "allow"
        os_rule = rule.get("os")
        if os_rule is None:
            allowed = action
        elif os_rule.get("name") == "linux":
            allowed = action
    return allowed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--prepare-bin", type=Path, required=True)
    parser.add_argument("--out-zip", type=Path, required=True)
    args = parser.parse_args()

    repo_root = args.root.resolve()
    work_dir = args.work_dir.resolve()
    prepare_bin = args.prepare_bin.resolve()
    out_zip = args.out_zip.resolve()

    lock = json.loads((repo_root / "research-lock.json").read_text(encoding="utf-8"))["minecraft"]
    mc_version = lock["version"]
    manifest_sha1 = lock["manifest_sha1"]
    client_sha1 = lock["client_sha1"]
    server_sha1 = lock["server_sha1"]

    print(f"[sim-base] Preparing pinned Minecraft {mc_version} inputs in {work_dir}")
    mc_dir = work_dir / "minecraft"
    version_json = mc_dir / "versions" / mc_version / f"{mc_version}.json"
    client_jar = mc_dir / "versions" / mc_version / f"{mc_version}.jar"
    server_jar = work_dir / "server.jar"
    libraries_dir = mc_dir / "libraries"
    assets_dir = mc_dir / "assets"

    manifest_url = f"https://piston-meta.mojang.com/v1/packages/{manifest_sha1}/{mc_version}.json"
    download_verified(manifest_url, version_json, manifest_sha1)
    vdoc = json.loads(version_json.read_text(encoding="utf-8"))

    download_verified(vdoc["downloads"]["client"]["url"], client_jar, client_sha1)
    download_verified(vdoc["downloads"]["server"]["url"], server_jar, server_sha1)

    # Download required libraries in parallel
    lib_tasks: list[tuple[str, Path, str]] = []
    for lib in vdoc.get("libraries", []):
        if not rule_applies_on_linux(lib):
            continue
        downloads = lib.get("downloads", {})
        artifact = downloads.get("artifact")
        if artifact and artifact.get("url") and artifact.get("path") and artifact.get("sha1"):
            lib_tasks.append((artifact["url"], libraries_dir / artifact["path"], artifact["sha1"]))
        classifiers = downloads.get("classifiers") or {}
        for key, nat in classifiers.items():
            if "linux" in key and nat.get("url") and nat.get("path") and nat.get("sha1"):
                lib_tasks.append((nat["url"], libraries_dir / nat["path"], nat["sha1"]))

    print(f"[sim-base] Fetching {len(lib_tasks)} Maven libraries…")
    with ThreadPoolExecutor(max_workers=12) as pool:
        futs = [pool.submit(download_verified, u, p, s) for u, p, s in lib_tasks]
        for fut in as_completed(futs):
            fut.result()

    # Download asset index + ONLY non-.ogg objects (fonts, Unifont, panorama, languages, built-in packs, sounds.json)
    asset_info = vdoc["assetIndex"]
    idx_id = asset_info["id"]
    idx_sha1 = asset_info["sha1"]
    idx_path = assets_dir / "indexes" / f"{idx_id}.json"
    download_verified(asset_info["url"], idx_path, idx_sha1)
    idx_doc = json.loads(idx_path.read_text(encoding="utf-8"))
    objects = idx_doc.get("objects", {})

    obj_tasks: list[tuple[str, Path, str]] = []
    sounds_json_hash: str | None = None
    for logical_name, meta in objects.items():
        h = meta["hash"]
        if logical_name == "minecraft/sounds.json":
            sounds_json_hash = h
        if logical_name.startswith("minecraft/sounds/") or logical_name.endswith(".ogg"):
            continue
        obj_url = f"https://resources.download.minecraft.net/{h[:2]}/{h}"
        obj_dest = assets_dir / "objects" / h[:2] / h
        obj_tasks.append((obj_url, obj_dest, h))

    print(f"[sim-base] Fetching {len(obj_tasks)} non-audio metadata/font/UI asset objects (0 .ogg files)…")
    with ThreadPoolExecutor(max_workers=16) as pool:
        futs = [pool.submit(download_verified, u, p, s) for u, p, s in obj_tasks]
        for fut in as_completed(futs):
            fut.result()

    output_dir = work_dir / "prepared-26.3"
    if output_dir.exists():
        shutil.rmtree(output_dir)

    cmd = [
        str(prepare_bin),
        "--prepare",
        "--root",
        str(repo_root),
        "--version-json",
        str(version_json),
        "--client-jar",
        str(client_jar),
        "--server-jar",
        str(server_jar),
        "--libraries-dir",
        str(libraries_dir),
        "--assets-dir",
        str(assets_dir),
        "--output",
        str(output_dir),
    ]
    print("[sim-base] Running horizon-prepare:", " ".join(cmd))
    subprocess.run(cmd, check=True)

    # Copy `minecraft/sounds.json` event table (JSON metadata only, no .ogg audio)
    if sounds_json_hash:
        src_sounds_json = assets_dir / "objects" / sounds_json_hash[:2] / sounds_json_hash
        dst_sounds_json = output_dir / "assets" / "minecraft" / "sounds.json"
        if src_sounds_json.is_file():
            dst_sounds_json.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src_sounds_json, dst_sounds_json)

    # Remove any accidental `.ogg` or preparation lock/scratch directories before packing
    lock_file = output_dir / ".horizon-prepare.lock"
    lock_file.unlink(missing_ok=True)
    for scratch_dir in [
        "client-classes",
        "entity-models",
        "mob-data",
        "entity-types",
        "block-behaviours",
        "datafix-tables",
        "creative-inventory",
        "gameplay-data",
    ]:
        candidate = output_dir / scratch_dir
        if candidate.is_dir():
            shutil.rmtree(candidate)
    for log_file in output_dir.glob("*-export.log"):
        log_file.unlink(missing_ok=True)
    for ogg in output_dir.rglob("*.ogg"):
        ogg.unlink()

    # Validate manifest & all required simulation tables
    manifest = json.loads((output_dir / "horizon-resources.json").read_text(encoding="utf-8"))
    if manifest.get("schema") != 7:
        raise RuntimeError(f"Unexpected schema in horizon-resources.json: {manifest.get('schema')}")
    if not isinstance(manifest.get("data_version"), int):
        raise RuntimeError("Missing integer data_version in horizon-resources.json")
    for rel in REQUIRED_TOP_FILES + REQUIRED_NESTED_FILES:
        if not (output_dir / rel).is_file():
            raise RuntimeError(f"Prepared output missing required engine file: {rel}")

    # Pack deterministic, symlink-free `sim-base-26.3.zip`
    out_zip.parent.mkdir(parents=True, exist_ok=True)
    tmp_zip = out_zip.with_suffix(".zip.tmp")
    tmp_zip.unlink(missing_ok=True)
    all_files = sorted(p for p in output_dir.rglob("*") if p.is_file() and not p.is_symlink())
    print(f"[sim-base] Packing {len(all_files)} files into {out_zip}…")
    with zipfile.ZipFile(tmp_zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
        for file_path in all_files:
            rel_posix = file_path.relative_to(output_dir).as_posix()
            info = zipfile.ZipInfo(rel_posix, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            with file_path.open("rb") as src, zf.open(info, "w") as dst:
                shutil.copyfileobj(src, dst, length=256 * 1024)
    tmp_zip.replace(out_zip)

    digest = sha256_file(out_zip)
    sha_file = out_zip.with_name(out_zip.name + ".sha256")
    sha_file.write_text(f"{digest}  {out_zip.name}\n", encoding="utf-8")
    size_mb = out_zip.stat().st_size / (1024 * 1024)
    print(f"[sim-base] Done: {out_zip.name} ({size_mb:.2f} MiB, sha256={digest})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
