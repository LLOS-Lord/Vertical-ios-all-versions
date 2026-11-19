#!/usr/bin/env python3
"""Build and verify a *real* per-version Rust simulation pack.

The iOS "all versions" tree previously wrote `horizon-resources.json` with
`source_minecraft: "1.20.4"` but `minecraft: "26.3"`, `data_version: 5023`
and hard-linked 26.3 registries/items/worldgen. That is a label swap: the world
still runs 26.3. This tool refuses that shape.

A playable non-26.3 version requires:
1. a prepared root made for THAT version (its own `horizon-resources.json`,
   `registries.json`, `items.json`, `worldgen/`, ... and its own data_version);
2. a `horizon-data.bin` artifact built from those bytes;
3. a `rust-translation-report.json` produced by `tools/version_port.py` after
   regenerating the Rust sources for that version and running the parity
   harnesses, naming the rebuilt engine binary and generated Rust sources.

No Mojang files are downloaded or committed here. The pack stores hashes and
facts only. Run `--self-test` for a host-side contract test; it uses a temporary
synthetic fixture and does not claim that fixture is a real Minecraft version.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path

ENGINE_PIN = "26.3"
ENGINE_DATA_VERSION = 5023
SCHEMA = 1

# Same contract as crates/horizon-client/src/versions.rs REQUIRED_SIMULATION_FILES.
REQUIRED_SIMULATION_FILES = [
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
    "assets/minecraft/font/default.json",
]


def fnv1a64(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        raise ValueError(f"{path}: {e}") from e


def is_real_hash(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(c in "0123456789abcdefABCDEF" for c in value)
        and any(c != "0" for c in value)
    )


def combined(root: Path, names: list[str]) -> bytes:
    out = bytearray()
    for name in names:
        p = root / name
        if not p.is_file():
            raise ValueError(f"missing required table: {p}")
        out.extend(p.read_bytes())
    return bytes(out)


def worldgen_bytes(root: Path) -> bytes:
    out = bytearray()
    folder = root / "worldgen"
    if not folder.is_dir():
        raise ValueError(f"missing worldgen folder: {folder}")
    for p in sorted(folder.rglob("*")):
        if p.is_file():
            out.extend(p.relative_to(root).as_posix().encode())
            out.extend(p.read_bytes())
    return bytes(out)


def validate_prepared_root(root: Path, version_id: str) -> dict:
    if version_id == ENGINE_PIN:
        raise ValueError(f"{ENGINE_PIN} is the pinned native engine and needs no version pack")
    manifest = read_json(root / "horizon-resources.json")
    label = manifest.get("source_minecraft") or manifest.get("minecraft")
    engine = manifest.get("minecraft")
    sim = manifest.get("simulation_version")
    if label != version_id or engine != version_id or sim != version_id:
        raise ValueError(
            "prepared root is not really for this version: "
            f"source_minecraft={label!r}, minecraft={engine!r}, simulation_version={sim!r}"
        )
    dv = manifest.get("data_version")
    protocol = manifest.get("protocol")
    if not isinstance(dv, int) or dv <= 0:
        raise ValueError("data_version must be a positive integer from this version's own jar")
    if dv == ENGINE_DATA_VERSION:
        raise ValueError(
            f"data_version {dv} is the pinned 26.3 world format; refusing a 26.3 table set under label {version_id}"
        )
    if not isinstance(protocol, int) or protocol <= 0:
        raise ValueError("protocol must be a positive integer from this version's own jar")
    if manifest.get("full_simulation") is not True:
        raise ValueError("full_simulation must be true for a translated version pack")
    for rel in REQUIRED_SIMULATION_FILES + REQUIRED_NESTED_FILES:
        if not (root / rel).is_file():
            raise ValueError(f"missing required table: {rel}")
    return {"data_version": dv, "protocol": protocol}


def validate_translation_report(path: Path, version_id: str, facts: dict) -> dict:
    report = read_json(path)
    if report.get("schema") != SCHEMA:
        raise ValueError(f"{path}: unknown rust-translation-report schema")
    if report.get("id") != version_id or report.get("simulation_version") != version_id:
        raise ValueError(f"{path}: report names a different version")
    if report.get("engine_pin") != ENGINE_PIN:
        raise ValueError(f"{path}: report was not built against the pinned {ENGINE_PIN} engine")
    if report.get("data_version") != facts["data_version"]:
        raise ValueError(f"{path}: data_version disagrees with horizon-resources.json")
    if report.get("protocol") != facts["protocol"]:
        raise ValueError(f"{path}: protocol disagrees with horizon-resources.json")
    if report.get("parity_harness_passed") is not True:
        raise ValueError(f"{path}: parity harnesses did not pass for this version")
    for key in ("engine_build_sha256", "rust_sources_sha256"):
        if not is_real_hash(report.get(key)):
            raise ValueError(f"{path}: {key} must be a real, non-zero SHA-256")
    return report


def build_pack(
    root: Path,
    version_id: str,
    artifact: Path,
    report_path: Path,
    output: Path,
) -> dict:
    facts = validate_prepared_root(root, version_id)
    report = validate_translation_report(report_path, version_id, facts)
    artifact_bytes = artifact.read_bytes()
    registries = combined(root, ["registries.json", "entity-types.json", "command-registries.json"])
    items = combined(root, ["items.json", "creative-inventory.json", "model-states.json"])
    worldgen = worldgen_bytes(root)
    try:
        artifact_rel = artifact.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        artifact_rel = artifact.name
    pack = {
        "schema": SCHEMA,
        "id": version_id,
        "simulation_version": version_id,
        "minecraft": version_id,
        "data_version": facts["data_version"],
        "protocol": facts["protocol"],
        "engine_pin": ENGINE_PIN,
        "engine_build_sha256": report["engine_build_sha256"],
        "rust_sources_sha256": report["rust_sources_sha256"],
        "parity_harness_passed": True,
        "registries_digest": fnv1a64(registries),
        "items_digest": fnv1a64(items),
        "worldgen_digest": fnv1a64(worldgen),
        "artifact_digest": fnv1a64(artifact_bytes),
        "artifact_sha256": sha256_hex(artifact_bytes),
        "artifact_path": artifact_rel,
        "built_by": f"tools/prepare_version_pack.py --version {version_id}",
        "translation_report": report_path.name,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    tmp = output.with_name(output.name + ".tmp")
    tmp.write_text(json.dumps(pack, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, output)
    return pack


def check_pack(
    root: Path,
    version_id: str,
    artifact: Path,
    pack_path: Path,
) -> None:
    facts = validate_prepared_root(root, version_id)
    pack = read_json(pack_path)
    if pack.get("schema") != SCHEMA:
        raise ValueError("unknown version-pack schema")
    for key in ("id", "simulation_version", "minecraft"):
        if pack.get(key) != version_id:
            raise ValueError(f"version-pack {key} disagrees with {version_id}")
    if pack.get("engine_pin") != ENGINE_PIN:
        raise ValueError("version-pack was built against a different engine pin")
    if pack.get("data_version") != facts["data_version"] or pack.get("protocol") != facts["protocol"]:
        raise ValueError("version-pack facts disagree with the prepared root")
    if pack.get("parity_harness_passed") is not True:
        raise ValueError("version-pack has no passed parity harness")
    for key in ("engine_build_sha256", "rust_sources_sha256"):
        if not is_real_hash(pack.get(key)):
            raise ValueError(f"version-pack {key} is not a real SHA-256")
    artifact_bytes = artifact.read_bytes()
    if fnv1a64(artifact_bytes) != pack.get("artifact_digest"):
        raise ValueError("artifact FNV digest mismatch")
    if sha256_hex(artifact_bytes) != pack.get("artifact_sha256"):
        raise ValueError("artifact SHA-256 mismatch")
    if fnv1a64(combined(root, ["registries.json", "entity-types.json", "command-registries.json"])) != pack.get("registries_digest"):
        raise ValueError("registries digest mismatch")
    if fnv1a64(combined(root, ["items.json", "creative-inventory.json", "model-states.json"])) != pack.get("items_digest"):
        raise ValueError("items digest mismatch")
    if fnv1a64(worldgen_bytes(root)) != pack.get("worldgen_digest"):
        raise ValueError("worldgen digest mismatch")


def synthetic_fixture(base: Path, version_id: str = "1.20.4") -> tuple[Path, Path, Path]:
    """A *contract test* fixture, not a claim that these bytes are Minecraft 1.20.4."""
    root = base / "versions" / version_id / "resources"
    for rel in REQUIRED_SIMULATION_FILES + REQUIRED_NESTED_FILES:
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(f"{version_id}/{rel}".encode())
    (root / "horizon-resources.json").write_text(
        json.dumps(
            {
                "schema": 7,
                "minecraft": version_id,
                "source_minecraft": version_id,
                "simulation_version": version_id,
                "data_version": 3700,
                "protocol": 765,
                "full_simulation": True,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    artifact = root / "horizon-data.bin"
    artifact.write_bytes(b"prepared-artifact-for-1.20.4-contract-test")
    report = root / "rust-translation-report.json"
    report.write_text(
        json.dumps(
            {
                "schema": SCHEMA,
                "id": version_id,
                "simulation_version": version_id,
                "engine_pin": ENGINE_PIN,
                "data_version": 3700,
                "protocol": 765,
                "parity_harness_passed": True,
                "engine_build_sha256": "a" * 64,
                "rust_sources_sha256": "b" * 64,
                "note": "SYNTHETIC TEST FIXTURE ONLY — not a real Minecraft version",
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return root, artifact, report


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="horizon-version-pack-") as td:
        base = Path(td)
        root, artifact, report = synthetic_fixture(base)
        pack_path = root / "version-pack.json"
        pack = build_pack(root, "1.20.4", artifact, report, pack_path)
        assert pack["items_digest"] != fnv1a64(b"26.3/items")
        check_pack(root, "1.20.4", artifact, pack_path)

        # A fake 26.3 manifest under a foreign label must be refused.
        fake = root / "horizon-resources.json"
        fake.write_text(
            json.dumps(
                {
                    "minecraft": ENGINE_PIN,
                    "source_minecraft": "1.20.4",
                    "data_version": ENGINE_DATA_VERSION,
                    "full_simulation": True,
                }
            ),
            encoding="utf-8",
        )
        try:
            validate_prepared_root(root, "1.20.4")
        except ValueError as e:
            assert "really for this version" in str(e) or "26.3" in str(e)
        else:
            raise AssertionError("fake 26.3 manifest was accepted")

        # Restore, then prove that a failed parity harness is refused.
        synthetic_fixture(base)
        report.write_text(
            json.dumps(
                {
                    "schema": SCHEMA,
                    "id": "1.20.4",
                    "simulation_version": "1.20.4",
                    "engine_pin": ENGINE_PIN,
                    "data_version": 3700,
                    "protocol": 765,
                    "parity_harness_passed": False,
                    "engine_build_sha256": "a" * 64,
                    "rust_sources_sha256": "b" * 64,
                }
            ),
            encoding="utf-8",
        )
        try:
            build_pack(root, "1.20.4", artifact, report, pack_path)
        except ValueError as e:
            assert "parity" in str(e)
        else:
            raise AssertionError("pack without parity proof was accepted")

        # A tampered artifact must fail --check.
        synthetic_fixture(base)
        pack = build_pack(root, "1.20.4", artifact, report, pack_path)
        artifact.write_bytes(b"tampered")
        try:
            check_pack(root, "1.20.4", artifact, pack_path)
        except ValueError as e:
            assert "artifact" in str(e)
        else:
            raise AssertionError("tampered artifact was accepted")
    print("prepare_version_pack self-test: OK (7 contract checks)")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", type=Path, help="prepared resources root for one version")
    ap.add_argument("--version", help="exact Minecraft version id, e.g. 1.20.4")
    ap.add_argument("--artifact", type=Path, help="prepared artifact (default <root>/horizon-data.bin)")
    ap.add_argument("--report", type=Path, help="Rust translation + parity report")
    ap.add_argument("--output", type=Path, help="version-pack.json output (default inside root)")
    ap.add_argument("--check", action="store_true", help="verify an existing version-pack.json instead of writing it")
    ap.add_argument("--self-test", action="store_true", help="run host-side contract tests and exit")
    args = ap.parse_args(argv)
    if args.self_test:
        self_test()
        return 0
    if not args.root or not args.version:
        ap.error("--root and --version are required unless --self-test")
    root = args.root
    artifact = args.artifact or root / "horizon-data.bin"
    output = args.output or root / "version-pack.json"
    report = args.report or root / "rust-translation-report.json"
    try:
        if args.check:
            check_pack(root, args.version, artifact, output)
            print(f"OK: {output} binds {args.version} to verified per-version tables")
        else:
            pack = build_pack(root, args.version, artifact, report, output)
            print(f"OK: wrote {output} for real version {args.version} (data_version={pack['data_version']})")
        return 0
    except (OSError, ValueError) as e:
        print(f"REFUSED: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
