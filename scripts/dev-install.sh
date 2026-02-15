#!/usr/bin/env bash

# This script requires `rustup` and `jq` to be installed

set -o errexit # make script exit when a command fails
set -o nounset # make script exit when using undeclared variables
set -o pipefail # make script exit when command fails in a pipe
set -o xtrace # print a trace of all commands executed by script

if [ "$#" -ne 1 ]; then
    >&2 echo "Illegal number of parameters [$#]"
    >&2 echo "usage: dev-install.sh [toolchain]"
    exit 1
fi

TOOLCHAIN="${1}"

if ! command -v rustup &> /dev/null
then
    >&2 echo "rustup could not be found, please install it"
    exit
fi

if ! command -v jq &> /dev/null
then
    >&2 echo "jq could not be found, please install it"
    exit
fi

rustup toolchain install "${TOOLCHAIN}"
if [ "${TOOLCHAIN}" == "nightly" ]; then
    VERSION=$(cargo metadata --format-version 1 | jq -r '.packages[] | select(.name=="ribs") | .metadata."kani-verifier".version')
    if [ -z "$VERSION" ]; then
        >&2 echo "Could not find kani-verifier version in Cargo.toml"
        exit 1
    fi
    cargo install --locked kani-verifier --version "${VERSION}"
    cargo kani setup
fi