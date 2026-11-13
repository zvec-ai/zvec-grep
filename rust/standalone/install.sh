#!/bin/sh
# Download a complete native bundle; the executable owns the activation transaction.
set -eu
set -f

fail() { printf 'zvec-grep: %s\n' "$*" >&2; exit 1; }
validate_manifest() {
    LC_ALL=C awk -F '\t' '
    function invalid(reason) { print "invalid release manifest: " reason > "/dev/stderr"; failed=1; exit 1 }
    {
        sub(/\r$/, "")
        rows[NR]=$0
        if ($1 ~ /^(schema|version|tag|commit|partial)$/) {
            if (NF != 2 || $1 in metadata) invalid("invalid or duplicate release " $1)
            metadata[$1]=$2
        } else {
            if (NF != 5) invalid("invalid archive columns")
            key=$1 "/" $2
            if ($1 !~ /^[a-z0-9]+(-[a-z0-9]+)+$/ || $2 !~ /^(cpu|cuda)$/ || key in identities ||
                length($4) != 64 || $4 ~ /[^a-f0-9]/ || $5 !~ /^[1-9][0-9]*$/ || $5+0 > 9007199254740991)
                invalid("invalid or duplicate archive metadata")
            identities[key]=1
            count++
            targets[count]=$1; variants[count]=$2; files[count]=$3
        }
    }
    END {
        if (failed) exit 1
        if (metadata["schema"] !~ /^1$/) invalid("unsupported schema")
        if (metadata["version"] !~ /^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$/) invalid("invalid release version")
        if (metadata["tag"] !~ /^[A-Za-z0-9][A-Za-z0-9._-]*$/) invalid("invalid release tag")
        if (length(metadata["commit"]) != 40 || metadata["commit"] ~ /[^a-f0-9]/) invalid("invalid source commit")
        if (metadata["partial"] !~ /^(true|false)$/) invalid("invalid partial flag")
        if (!count) invalid("missing archives")
        for (i=1; i<=count; i++) {
            extension=targets[i] ~ /^win32-/ ? "zip" : "tar.gz"
            if (files[i] != "zvec-grep-" metadata["version"] "-" targets[i] "-" variants[i] "." extension)
                invalid("invalid archive filename")
        }
        for (i=1; i<=NR; i++) print rows[i]
    }' "$1"
}

check_version() {
    "$1" --version > "$staging/version" 2> "$staging/version-error" &
    version_pid=$!
    (sleep 10; kill -KILL "$version_pid" 2>/dev/null || :) >/dev/null 2>&1 &
    version_watchdog=$!
    version_status=0
    wait "$version_pid" || version_status=$?
    kill "$version_watchdog" 2>/dev/null || :
    wait "$version_watchdog" 2>/dev/null || :
    [ "$version_status" = 0 ] && [ "$(cat "$staging/version")" = "$2" ]
}
usage() {
    cat <<'EOF'
Usage: install.sh [--version VERSION] [--accelerator auto|cpu|cuda]
                  [--install-dir PATH] [--home PATH]
                  [--token-file PATH]
                  [--skip-agent-upgrade] [--activate VERSION | --uninstall]
Re-run to upgrade. CUDA plugins require compatible host drivers and runtime libraries.
Programs: ~/.zvec-grep/dist; command link: ~/.local/bin/zg. Shell configuration is never modified.
EOF
}

install_dir=${ZG_INSTALL_DIR:-"$HOME/.zvec-grep/dist"}
version=
activate=
repository=${ZG_INSTALL_REPOSITORY:-}
accelerator=${ZG_INSTALL_ACCELERATOR:-}
skip_agents=${ZG_INSTALL_SKIP_AGENT_UPGRADE:-0}
remove=0
daemon_home=
token_file=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        # Retain the earlier opt-out spelling; shell configuration is now always user-managed.
        --no-modify-path) shift ;;
        --skip-agent-upgrade) skip_agents=1; shift ;;
        --uninstall) remove=1; shift ;;
        --version|--activate|--accelerator|--install-dir|--home|--token-file)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"
            case "$1" in
                --version) version=$2 ;;
                --activate) activate=$2 ;;
                --accelerator) accelerator=$2 ;;
                --install-dir) install_dir=$2 ;;
                --home) daemon_home=$2 ;;
                --token-file) token_file=$2 ;;
            esac
            shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
case "$install_dir" in /*) ;; *) install_dir="$PWD/$install_dir" ;; esac
[ -z "$activate" ] || { [ -z "$version" ] && [ "$remove" = 0 ]; } || fail "--activate cannot be combined with --version or --uninstall"
state="$install_dir/install-state.tsv"
read_state() { if [ -f "$state" ]; then awk -F '\t' -v key="$1" '$1 == key {print $2; exit}' "$state"; fi; }
[ -n "$accelerator" ] || accelerator=$(read_state accelerator)
[ -n "$repository" ] || repository=$(read_state repository)
repository=${repository:-zvec-ai/zvec-grep}
printf '%s\n' "$repository" | LC_ALL=C grep -Eq '^[A-Za-z0-9_-][A-Za-z0-9_.-]*/[A-Za-z0-9_-][A-Za-z0-9_.-]*$' || fail "repository must be a GitHub owner/name"
base_url="https://github.com/$repository/releases"
# The test server implements GitHub's release URL layout; it is never a persisted source.
if [ -n "${ZG_INSTALL_TEST_RELEASE_URL:-}" ]; then
    printf '%s\n' "$ZG_INSTALL_TEST_RELEASE_URL" | LC_ALL=C grep -Eq '^http://127\.0\.0\.1:[0-9]+$' || fail "invalid test release server"
    base_url=$ZG_INSTALL_TEST_RELEASE_URL
fi
accelerator=${accelerator:-auto}
case "$accelerator" in auto|cpu|cuda) ;; *) fail "accelerator must be auto, cpu or cuda" ;; esac
case "$version" in ''|[0-9]*) ;; *) fail "invalid version" ;; esac
case "$version" in *[!A-Za-z0-9.-]*) fail "invalid version" ;; esac
if [ "${CI:-}" = true ] || [ "${CI:-}" = 1 ] || [ "$(id -u)" = 0 ] || [ -n "${SUDO_USER:-}" ]; then skip_agents=1; fi

os=$(uname -s)
arch=$(uname -m)
case "$arch" in aarch64|arm64) arch=arm64 ;; x86_64|amd64) arch=x64 ;; *) fail "unsupported architecture: $arch" ;; esac
case "$os" in
    Darwin)
        # Prefer the native architecture when invoked from a translated terminal.
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || :)" = 1 ]; then arch=arm64; fi
        target="darwin-$arch"
        if [ "$arch" = x64 ]; then
            major=$(sw_vers -productVersion | cut -d . -f 1)
            [ "$major" -ge 14 ] || fail "Intel macOS requires version 14 or newer"
        fi ;;
    Linux)
        libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || :)
        case "$libc" in 'glibc '*) ;; *) fail "Linux requires glibc; musl is not supported" ;; esac
        printf '%s\n' "$libc" | awk '{split($2,v,"."); exit !(v[1]>2 || (v[1]==2 && v[2]>=28))}' || fail "glibc 2.28 or newer is required"
        target="linux-$arch-gnu" ;;
    *) fail "unsupported OS: $os; use install.ps1 on Windows" ;;
esac
temp_base=${ZG_INSTALL_TMPDIR:-${TMPDIR:-/tmp}}
mkdir -p "$temp_base"
staging=$(mktemp -d "$temp_base/zg-install.XXXXXXXX")
trap 'rm -rf "$staging"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error --connect-timeout 20 --max-time 600 --retry 2 --output "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --timeout=60 --tries=3 -O "$2" "$1"
    else fail "curl or wget is required"; fi
}
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
    else fail "sha256sum or shasum is required"; fi
}
has_nvidia() {
    command -v nvidia-smi >/dev/null 2>&1 || return 1
    nvidia-smi --query-gpu=name,driver_version --format=csv,noheader >"$staging/nvidia" 2>/dev/null &
    probe_pid=$!
    (sleep 5; kill -KILL "$probe_pid" 2>/dev/null || :) &
    timer_pid=$!
    result=0
    wait "$probe_pid" || result=$?
    kill "$timer_pid" 2>/dev/null || :
    wait "$timer_pid" 2>/dev/null || :
    [ "$result" = 0 ] && [ -s "$staging/nvidia" ]
}

cached=0
if [ "$remove" = 1 ] || [ -n "$activate" ]; then
    bundle=$(read_state bundle)
    printf '%s\n' "$bundle" | LC_ALL=C grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?/[A-Za-z0-9-]+-(cpu|cuda)$' || fail "no valid standalone installation at $install_dir"
    source_dir="$install_dir/$bundle"
    if [ -f "$source_dir/bin/zg" ]; then
        cp -R "$source_dir" "$staging/zg"
        if check_version "$staging/zg/bin/zg" "$(read_state version)"; then cached=1; fi
    fi
    if [ "$cached" = 0 ]; then
        [ -z "$activate" ] || fail "cached executable is unavailable; repair the installation first"
        # A committed uninstall may already have deleted part of the cached runtime.
        rm -rf "$staging/zg"
        version=$(read_state version)
    fi
fi
if [ "$cached" = 0 ]; then
    command -v tar >/dev/null 2>&1 || fail "tar is required"
    if [ -n "$version" ]; then manifest_url="$base_url/download/v$version/manifest.tsv"
    else manifest_url="$base_url/latest/download/manifest.tsv"; fi
    download "$manifest_url" "$staging/manifest.download"
    validate_manifest "$staging/manifest.download" > "$staging/manifest.tsv" || fail "invalid release manifest"
    selected_version=$(awk -F '\t' '$1=="version" {print $2}' "$staging/manifest.tsv")
    [ -z "$version" ] || [ "$version" = "$selected_version" ] || fail "requested version does not match release manifest"
    tag=$(awk -F '\t' '$1=="tag" {print $2}' "$staging/manifest.tsv")
    [ -z "$version" ] || [ "$tag" = "v$version" ] || fail "release tag does not match requested version"
    version=$selected_version
    variant=cpu
    if [ "$accelerator" = cuda ]; then variant=cuda
    elif [ "$accelerator" = auto ] && [ "$target" = linux-x64-gnu ] && has_nvidia; then variant=cuda; fi
    fetch_bundle() {
        row=$(awk -F '\t' -v target="$target" -v variant="$variant" '$1==target && $2==variant {print $3 " " $4 " " $5}' "$staging/manifest.tsv")
        [ "$(printf '%s\n' "$row" | wc -l | tr -d ' ')" = 1 ] || return 1
        set -- $row
        [ "$#" = 3 ] || return 1
        archive=$1
        expected_hash=$2
        expected_size=$3
        download "$base_url/download/$tag/$archive" "$staging/archive.tar.gz" || return 1
        [ "$(wc -c < "$staging/archive.tar.gz" | tr -d ' ')" = "$expected_size" ] || return 1
        [ "$(sha256 "$staging/archive.tar.gz")" = "$expected_hash" ]
    }
    if ! fetch_bundle; then
        if [ "$accelerator" = auto ] && [ "$variant" = cuda ]; then
            printf '%s\n' 'CUDA download failed verification; installing the CPU base.' >&2
            variant=cpu
            fetch_bundle || fail "CPU download or checksum failed"
        else fail "no valid $variant archive for $target; download or checksum failed"; fi
    fi
    tar -tzf "$staging/archive.tar.gz" > "$staging/paths"
    while IFS= read -r path; do
        case "$path" in zg|zg/|zg/*) ;; *) fail "unsafe archive root" ;; esac
        case "/$path/" in */../*|*/./*|*\\*) fail "unsafe archive path" ;; esac
    done < "$staging/paths"
    tar -tvzf "$staging/archive.tar.gz" > "$staging/types"
    LC_ALL=C awk 'substr($0,1,1)!="-" && substr($0,1,1)!="d" {exit 1}' "$staging/types" || fail "archive contains links or special files"
    tar -xzf "$staging/archive.tar.gz" -C "$staging"
    check_version "$staging/zg/bin/zg" "$version" || fail "staged executable version check failed or timed out"
fi

set -- --standalone --install-dir "$install_dir" --accelerator "$accelerator" --repository "$repository" --no-modify-path --launcher "$HOME/.local/bin/zg"
[ "$remove" = 0 ] || set -- "$@" --remove
[ -z "$activate" ] || set -- "$@" --activate "$activate"
[ "$skip_agents" = 0 ] || set -- "$@" --skip-agent-upgrade
[ -z "$daemon_home" ] || set -- "$@" --home "$daemon_home"
[ -z "$token_file" ] || set -- "$@" --token-file "$token_file"
"$staging/zg/bin/zg" "$@"

# A fixed command link keeps PATH and absolute client launchers valid across updates.
[ "$remove" = 0 ] || exit 0
path_dir="$HOME/.local/bin"
printf 'Command entry: %s/zg\n' "$path_dir"
case ":${PATH:-}:" in
    *":$path_dir:"*|*":$path_dir/:"*) ;;
    *)
        printf '%s\n' 'Installation succeeded. The command directory is not in your current PATH.'
        printf '%s\n' 'Run the following in your shell and add it to the shell configuration you use:'
        case "${SHELL:-}" in
            */fish) printf '%s\n' 'fish_add_path "$HOME/.local/bin"' ;;
            *) cat <<'EOF'
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) export PATH="$HOME/.local/bin:$PATH" ;;
esac
EOF
            ;;
        esac
        ;;
esac
existing=$(command -v zg 2>/dev/null || :)
if [ -n "$existing" ] && [ "$existing" != "$path_dir/zg" ]; then
    printf 'This environment resolves zg to %s; place %s first in PATH to use this installation.\n' "$existing" "$path_dir"
fi
printf '%s\n' 'Verify in your terminal: command -v zg; zg --version'
