#!/usr/bin/env python3
# Prepare a ChromeOS Widevine CDM for a 4 KiB-page Linux userspace.
#
# Modern glibc refuses to load the ChromeOS CDM build because it uses DT_RELR
# relocations without the GLIBC_ABI_DT_RELR version dependency, and because it
# calls __aarch64_ldadd4_acq_rel / __aarch64_swp4_acq_rel, which current
# libgcc builds do not export. Asahi Linux's widevine_fixup.py injects both.
#
# The fixup keys its segment alignment off the host page size, so this wrapper
# selects 4 KiB explicitly, which is what Scarlet's aarch64 MMU and the
# Scarlet Debian view use. That path does not weaken RELRO or W^X.
#
# usage: prepare-cdm.py INPUT.so OUTPUT.so

import mmap
import runpy
import sys
from pathlib import Path


def main():
    if len(sys.argv) != 3:
        print("usage: prepare-cdm.py INPUT.so OUTPUT.so")
        return 1
    mmap.PAGESIZE = 4096
    script = Path(__file__).with_name("widevine_fixup.py")
    sys.argv = [str(script), "-v", sys.argv[1], sys.argv[2]]
    runpy.run_path(str(script), run_name="__main__")
    return 0


if __name__ == "__main__":
    sys.exit(main())
