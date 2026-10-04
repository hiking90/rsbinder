#!/bin/sh
# Regenerates sys.rs from the kernel UAPI headers under include/.
#
# include/linux/android/{binder,binderfs}.h are verbatim copies of
# kernel/common include/uapi/linux/android/ (branch android17-6.18,
# commit 6da35a4b1b02). Refresh them by copying, never by editing, then
# rerun this script: sys.rs must stay the unmodified bindgen output.
#
# Needs bindgen-cli (`cargo install bindgen-cli --version 0.73.2`; another
# version may emit different output), libclang and the host's
# kernel headers for <linux/types.h> and <linux/ioctl.h>.
#
# --no-layout-tests: bindgen's const layout assertions check alignment, and
# a u64 is 4-aligned on i686, so they would fail to compile there. The
# structs rsbinder decodes by hand carry their own size/offset assertions
# (binder_object.rs, transaction_data.rs).
# -D__packed: the UAPI source spells the attribute the way the kernel's
# own headers define it; headers_install output would have expanded it.
set -eu
cd "$(dirname "$0")"
bindgen include/linux/android/binderfs.h -o sys.rs \
    --rust-target 1.86 \
    --no-layout-tests \
    --no-doc-comments \
    --no-prepend-enum-name \
    --allowlist-file '.*/linux/android/binder(fs)?\.h' \
    --allowlist-var '_IOC_(NR|SIZE)(MASK|SHIFT)' \
    -- -Iinclude '-D__packed=__attribute__((packed))'
