#!/usr/bin/env python3
# Licensed under the Apache-2.0 license
"""Build the firmware of the scenarios in scenarios.json and collect it in www/.

For each scenario this builds `<image>_image_emulator` in an OpenPRoT
checkout: the image signed for the emulator, with the Caliptra ROM and
firmware, the MCU ROM and the SoC manifest that boot it. The files are copied
to www/images/, named by content so that scenarios share what is the same,
and www/scenarios.json lists them for the page and the tests.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# The configuration OpenPRoT's `bazel test` builds firmware in (the `test`
# lines of its .bazelrc). `bazel cquery` applies those lines and `bazel build`
# does not, so both get them here and resolve the same output paths.
BAZEL_FLAGS = [
    "--platform_suffix=test",
    "--@pigweed//pw_kernel:enable_tests=true",
    # bazel/boot_files.bzl, which puts an image's boot files in an output group.
    "--inject_repository=openprot_wasm_bazel=" + os.path.join(REPO, "bazel"),
]

# Keys of a .boot_env file, and the names the page knows the images by.
BOOT_ENV_IMAGES = {
    "CALIPTRA_ROM": "caliptra_rom",
    "CALIPTRA_FW": "caliptra_fw",
    "MCU_ROM": "mcu_rom",
    "MCU_FW": "mcu_fw",
    "SOC_MANIFEST": "soc_manifest",
}


def run(cmd, cwd, capture=False):
    print("+ " + " ".join(cmd), file=sys.stderr)
    result = subprocess.run(
        cmd, cwd=cwd, check=True, text=True, stdout=subprocess.PIPE if capture else None
    )
    return result.stdout if capture else None


def emulator_target(image):
    """The `caliptra_mcu_image` that `veer_system_image` defines for `image`."""
    return image + "_image_emulator"


def build(openprot, targets):
    run(
        ["bazel", "build"]
        + BAZEL_FLAGS
        + [
            "--aspects=@openprot_wasm_bazel//:boot_files.bzl%boot_files",
            "--output_groups=boot_files",
        ]
        + targets,
        cwd=openprot,
    )


def runfiles(openprot, targets):
    """Maps each target to {runfile short path: path under the execution root}."""
    expr = (
        'str(target.label) + "\\t" + "\\t".join([f.short_path + "=" + f.path '
        'for f in providers(target)["DefaultInfo"].default_runfiles.files.to_list()])'
    )
    out = run(
        ["bazel", "cquery"]
        + BAZEL_FLAGS
        + ["--output=starlark", "--starlark:expr=" + expr, "set(" + " ".join(targets) + ")"],
        cwd=openprot,
        capture=True,
    )
    files = {}
    for line in out.splitlines():
        label, *pairs = line.split("\t")
        # A label prints as @@//pkg:name; the root repository's prefix varies.
        files[label[label.index("//"):]] = dict(pair.split("=", 1) for pair in pairs)
    return files


def git(openprot, *args):
    return subprocess.run(
        ["git", "-C", openprot] + list(args), check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def store(path, images_dir, name):
    """Copies `path` into `images_dir` under a name that includes its hash."""
    with open(path, "rb") as f:
        data = f.read()
    digest = hashlib.sha256(data).hexdigest()
    file_name = f"{name}-{digest[:16]}.bin"
    with open(os.path.join(images_dir, file_name), "wb") as f:
        f.write(data)
    return {"path": "images/" + file_name, "size": len(data), "sha256": digest}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--openprot", required=True, help="OpenPRoT checkout to build in")
    parser.add_argument("--www", required=True, help="output directory (www/)")
    parser.add_argument("--only", nargs="*", help="ids of the scenarios to build (default: all)")
    args = parser.parse_args()
    openprot = os.path.abspath(args.openprot)

    with open(os.path.join(REPO, "scenarios.json")) as f:
        config = json.load(f)
    scenarios = config["scenarios"]
    if args.only:
        known = {s["id"] for s in scenarios}
        unknown = sorted(set(args.only) - known)
        if unknown:
            sys.exit(f"no scenario named {', '.join(unknown)} (have: {', '.join(sorted(known))})")
        scenarios = [s for s in scenarios if s["id"] in args.only]

    targets = [emulator_target(s["image"]) for s in scenarios]
    build(openprot, targets)
    files = runfiles(openprot, targets)
    execroot = run(["bazel", "info", "execution_root"], cwd=openprot, capture=True).strip()

    images_dir = os.path.join(args.www, "images")
    shutil.rmtree(images_dir, ignore_errors=True)
    os.makedirs(images_dir)

    built = []
    for scenario, target in zip(scenarios, targets):
        by_short_path = files[target]
        boot_env_path = next(p for p in by_short_path.values() if p.endswith(".boot_env"))
        with open(os.path.join(execroot, boot_env_path)) as f:
            boot_env = json.load(f)
        images = {}
        for key, name in BOOT_ENV_IMAGES.items():
            # An rlocation path: the workspace name, then the short path.
            short_path = boot_env[key].split("/", 1)[1]
            path = os.path.join(execroot, by_short_path[short_path])
            if not os.path.exists(path):
                sys.exit(
                    f"{path} was not built. `bazel build` and `bazel cquery` resolved "
                    "different configurations; check BAZEL_FLAGS against OpenPRoT's .bazelrc."
                )
            images[name] = store(path, images_dir, name.replace("_", "-"))
        built.append(
            {
                "id": scenario["id"],
                "title": scenario["title"],
                "description": scenario["description"],
                "driver": scenario["driver"],
                "image": scenario["image"],
                "vendor_pk_hash": boot_env["VENDOR_PK_HASH"],
                "images": images,
            }
        )
        print(f"{scenario['id']}: MCU firmware {images['mcu_fw']['size']} bytes", file=sys.stderr)

    rev = git(openprot, "rev-parse", "HEAD")
    pinned = config["openprot"]["rev"]
    if rev != pinned:
        print(f"WARNING: built OpenPRoT {rev}; scenarios.json pins {pinned}", file=sys.stderr)
    manifest = {
        "openprot": {
            "repo": config["openprot"]["repo"],
            "rev": rev,
            "pinned_rev": pinned,
            "modified": bool(git(openprot, "status", "--porcelain", "--untracked-files=no")),
        },
        "scenarios": built,
    }
    with open(os.path.join(args.www, "scenarios.json"), "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    print(f"Wrote {args.www}/scenarios.json: {', '.join(s['id'] for s in built)}", file=sys.stderr)


if __name__ == "__main__":
    main()
