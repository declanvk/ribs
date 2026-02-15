#!/usr/bin/env bash

set -o errexit # make script exit when a command fails
set -o nounset # make script exit when using undeclared variables
set -o pipefail # make script exit when command fails in a pipe
set -o xtrace # print a trace of all commands executed by script

RUN_EXPENSIVE_TESTS=false
TOOLCHAIN=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --expensive)
            RUN_EXPENSIVE_TESTS=true
            shift 1
            ;;
        *)
            if [ -z "${TOOLCHAIN}" ]; then
                TOOLCHAIN=$1
                shift 1
            else
                >&2 echo "Illegal parameter [$1]"
                >&2 echo "usage: ci.sh [--expensive] [toolchain]"
                exit 1
            fi
            ;;
    esac
done

if [ -z "${TOOLCHAIN}" ]; then
    TOOLCHAIN=$(rustup show active-toolchain | cut -d '-' -f 1)
fi

TOOLCHAIN_ARG="+${TOOLCHAIN}"

cargo "${TOOLCHAIN_ARG}" fmt    -- --check
cargo "${TOOLCHAIN_ARG}" build  --all-targets

# --all-targets does not include the doctests
cargo "${TOOLCHAIN_ARG}" test   --lib --bins --examples --tests
cargo "${TOOLCHAIN_ARG}" test   --doc

cargo "${TOOLCHAIN_ARG}" test   --lib --bins --examples --tests --no-default-features

cargo "${TOOLCHAIN_ARG}" clippy --all-targets
cargo "${TOOLCHAIN_ARG}" doc    --no-deps --document-private-items

if [ "${RUN_EXPENSIVE_TESTS}" = "true" ]; then
    # Run expensive tests, such as loom and miri
    RUSTFLAGS="--cfg loom" cargo "${TOOLCHAIN_ARG}" test --lib --release

    if [ "${TOOLCHAIN}" = "nightly" ]; then
        cargo "${TOOLCHAIN_ARG}" miri test --lib --bins --examples --tests 
    fi
fi