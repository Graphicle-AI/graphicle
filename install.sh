#!/bin/sh
# Install Graphicle's two programs, graphicle and graphicle-graph, from a
# GitHub release of graphicle-ai/graphicle:
#
#   curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh -s -- --version 0.1.0
#
# It picks the archive for this machine (macOS or Linux with glibc, on arm64 or
# x86-64), downloads graphicle-<version>-<target>.tar.gz and its .sha256,
# verifies the checksum and copies both programs into ~/.local/bin. It never
# uses sudo and changes nothing outside that folder.
#
# Options (or the environment variable beside each):
#   --version VERSION  GRAPHICLE_VERSION   the release to install (default: the latest)
#   --prefix DIR       GRAPHICLE_PREFIX    install into DIR/bin (default: ~/.local)
#   --bin-dir DIR      GRAPHICLE_BIN_DIR   install into DIR itself
#   --base-url URL     GRAPHICLE_BASE_URL  folder holding the release files
#                                          (default: the GitHub release of VERSION)
#
# To uninstall, run `graphicle uninstall`, which removes what Graphicle added
# to your agents and repositories, then delete the two programs from that folder.
set -eu

REPOSITORY="graphicle-ai/graphicle"

say() {
    printf 'graphicle-install: %s\n' "$*" >&2
}

die() {
    say "error: $*"
    exit 1
}

usage() {
    cat <<EOF
Install graphicle and graphicle-graph from a release of $REPOSITORY.

Usage: install.sh [--version VERSION] [--prefix DIR | --bin-dir DIR] [--base-url URL]

  --version VERSION  the release to install (default: the latest)    GRAPHICLE_VERSION
  --prefix DIR       install into DIR/bin (default: ~/.local)        GRAPHICLE_PREFIX
  --bin-dir DIR      install into DIR itself                         GRAPHICLE_BIN_DIR
  --base-url URL     folder holding the release files                GRAPHICLE_BASE_URL
                     (default: the GitHub release of VERSION)

The archive's SHA-256 sum is checked before anything is installed. sudo is
never used. To uninstall, run 'graphicle uninstall', then delete the two
programs from that folder.

After installing: cd your-repo && graphicle setup
EOF
}

has() {
    command -v "$1" >/dev/null 2>&1
}

# download URL FILE
download() {
    case "$1" in
        https://*) secure="--proto =https --tlsv1.2" ;;
        *) secure="" ;;
    esac
    if has curl; then
        # shellcheck disable=SC2086
        curl $secure --fail --silent --show-error --location --retry 3 --output "$2" "$1" ||
            die "could not download $1"
    elif has wget; then
        case "$1" in
            https://*) wget --quiet --https-only --tries=3 --output-document="$2" "$1" ||
                die "could not download $1" ;;
            *) wget --quiet --tries=3 --output-document="$2" "$1" || die "could not download $1" ;;
        esac
    else
        die "this installer needs curl or wget"
    fi
}

# The version of the latest release, from the address /releases/latest
# redirects to (.../releases/tag/v<version>); no API call, so no rate limit.
latest_version() {
    url="https://github.com/$REPOSITORY/releases/latest"
    if has curl; then
        final=$(curl --proto =https --tlsv1.2 --fail --silent --location --head \
            --output /dev/null --write-out '%{url_effective}' "$url") ||
            die "found no published release at $url (or could not reach it); pass --version"
    elif has wget; then
        final=$(wget --https-only --spider --server-response "$url" 2>&1 |
            sed -n 's/^ *Location: *\([^ ]*\).*/\1/p' | tail -n 1)
    else
        die "this installer needs curl or wget"
    fi
    tag=${final##*/}
    case "$tag" in
        v[0-9]*) printf '%s\n' "${tag#v}" ;;
        *) die "found no published release at $url (it led to '$final'); pass --version" ;;
    esac
}

sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif has openssl; then
        openssl dgst -sha256 "$1" | sed 's/^.*= *//'
    else
        die "verifying the download needs sha256sum, shasum or openssl"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin)
            system=apple-darwin
            # A shell running under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            ;;
        Linux)
            system=unknown-linux-gnu
            if (ldd --version 2>&1 || true) | grep -qi musl; then
                die "this Linux uses musl (for example Alpine); the prebuilt programs need glibc, so build from source: https://github.com/$REPOSITORY"
            fi
            ;;
        *) die "no prebuilt programs for $os; build from source: https://github.com/$REPOSITORY" ;;
    esac
    case "$arch" in
        arm64 | aarch64) cpu=aarch64 ;;
        x86_64 | amd64) cpu=x86_64 ;;
        *) die "no prebuilt programs for $arch; build from source: https://github.com/$REPOSITORY" ;;
    esac
    printf '%s-%s\n' "$cpu" "$system"
}

main() {
    version=${GRAPHICLE_VERSION:-}
    prefix=${GRAPHICLE_PREFIX:-}
    bin_dir=${GRAPHICLE_BIN_DIR:-}
    base_url=${GRAPHICLE_BASE_URL:-}
    while [ $# -gt 0 ]; do
        case "$1" in
            --version) [ $# -ge 2 ] || die "--version needs a value"; version=$2; shift 2 ;;
            --version=*) version=${1#*=}; shift ;;
            --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; prefix=$2; shift 2 ;;
            --prefix=*) prefix=${1#*=}; shift ;;
            --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value"; bin_dir=$2; shift 2 ;;
            --bin-dir=*) bin_dir=${1#*=}; shift ;;
            --base-url) [ $# -ge 2 ] || die "--base-url needs a value"; base_url=$2; shift 2 ;;
            --base-url=*) base_url=${1#*=}; shift ;;
            -h | --help) usage; exit 0 ;;
            *) die "unknown option $1 (see --help)" ;;
        esac
    done

    target=$(detect_target)
    version=${version#v}
    if [ -z "$version" ]; then
        version=$(latest_version)
    fi
    case "$version" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) die "'$version' is not a release version such as 0.1.0" ;;
    esac
    if [ -z "$bin_dir" ]; then
        if [ -z "$prefix" ]; then
            [ -n "${HOME:-}" ] || die "HOME is not set; pass --bin-dir"
            prefix="$HOME/.local"
        fi
        bin_dir="$prefix/bin"
    fi
    base_url=${base_url:-"https://github.com/$REPOSITORY/releases/download/v$version"}
    base_url=${base_url%/}
    archive="graphicle-$version-$target.tar.gz"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t graphicle-install)
    [ -n "$tmp" ] && [ -d "$tmp" ] || die "could not create a temporary folder"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM

    say "downloading Graphicle $version for $target"
    download "$base_url/$archive" "$tmp/$archive"
    download "$base_url/$archive.sha256" "$tmp/$archive.sha256"

    expected=$(sed -n '1s/^\([0-9a-fA-F]\{64\}\)[ *][ *]*\([^ ]*\)$/\1 \2/p' "$tmp/$archive.sha256")
    listed=${expected#* }
    expected=$(printf '%s' "${expected%% *}" | tr 'A-F' 'a-f')
    [ ${#expected} -eq 64 ] || die "$archive.sha256 holds no SHA-256 sum"
    [ "${listed#\*}" = "$archive" ] || die "$archive.sha256 names '$listed', not $archive"
    actual=$(sha256_of "$tmp/$archive")
    [ "$actual" = "$expected" ] || die "checksum mismatch for $archive: expected $expected, got $actual; nothing was installed"
    say "checksum verified ($expected)"

    mkdir "$tmp/unpacked"
    tar -xzf "$tmp/$archive" -C "$tmp/unpacked" || die "could not unpack $archive"
    unpacked="$tmp/unpacked/graphicle-$version-$target"
    for program in graphicle graphicle-graph; do
        [ -f "$unpacked/$program" ] || die "$archive holds no $program"
    done

    mkdir -p "$bin_dir" || die "could not create $bin_dir; choose another folder with --bin-dir"
    [ -w "$bin_dir" ] || die "$bin_dir is not writable; choose another folder with --bin-dir (this installer never uses sudo)"
    for program in graphicle graphicle-graph; do
        # Copy beside the destination, then rename over it: a running program
        # keeps its old file, and an interrupted copy leaves no broken program.
        cp "$unpacked/$program" "$bin_dir/.$program.new.$$"
        chmod 755 "$bin_dir/.$program.new.$$"
        mv -f "$bin_dir/.$program.new.$$" "$bin_dir/$program"
    done
    say "installed graphicle and graphicle-graph in $bin_dir"

    if ! reported=$("$bin_dir/graphicle" --version 2>&1); then
        say "error: $bin_dir/graphicle does not run here: $reported"
        case "$target" in
            *-linux-gnu) say "the Linux programs need glibc 2.35 or later (Ubuntu 22.04, Debian 12, Fedora 36 or newer)" ;;
        esac
        exit 1
    fi
    say "$reported"
    case ":${PATH:-}:" in
        *":$bin_dir:"*) ;;
        *) say "add $bin_dir to your PATH, for example: export PATH=\"$bin_dir:\$PATH\"" ;;
    esac
    say "By default, graphicle checks GitHub for a newer release at most once a day; to turn this off, run: graphicle config set update_notifier false"
    say "Next: cd your-repo && graphicle setup"
}

main "$@"
