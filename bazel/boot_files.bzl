# Licensed under the Apache-2.0 license

"""Aspect for building the boot files of an OpenPRoT `caliptra_mcu_image`.

The default output of a `caliptra_mcu_image` is its `.boot_env` file. The
images that file names (Caliptra ROM and firmware, MCU ROM, MCU firmware, SoC
manifest) are runfiles, which `bazel build` builds only for executables. This
aspect puts them in an output group.

build.sh injects this directory into the OpenPRoT workspace as
`@openprot_wasm_bazel`.
"""

def _boot_files_impl(target, _ctx):
    return [OutputGroupInfo(boot_files = target[DefaultInfo].default_runfiles.files)]

boot_files = aspect(implementation = _boot_files_impl)
