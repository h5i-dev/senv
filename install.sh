#!/usr/bin/env sh
set -e

# Everything lives in main() and main is called on the last line, so a
# truncated `curl … | sh` cannot execute a half-downloaded prefix.
main() {
  REPO="h5i-dev/senv"
  BINARY="senv"
  INSTALL_DIR="${SENV_INSTALL_DIR:-/usr/local/bin}"

  # ── detect OS ──────────────────────────────────────────────────────────────
  OS="$(uname -s)"
  case "$OS" in
    Linux)  os="linux" ;;
    Darwin) os="macos" ;;
    *)
      echo "Unsupported OS: $OS" >&2
      echo "On Windows, install senv inside WSL2." >&2
      exit 1
      ;;
  esac

  # ── detect arch ────────────────────────────────────────────────────────────
  ARCH="$(uname -m)"
  case "$ARCH" in
    x86_64 | amd64)  arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *)
      echo "Unsupported architecture: $ARCH" >&2
      exit 1
      ;;
  esac

  # ── map to release target triple ───────────────────────────────────────────
  case "${os}-${arch}" in
    linux-x86_64)  target="x86_64-unknown-linux-musl" ;;
    linux-aarch64) target="aarch64-unknown-linux-musl" ;;
    macos-aarch64) target="aarch64-apple-darwin" ;;
    # Rosetta 2 translates x86_64 to arm64, not the reverse, so the published
    # Apple Silicon archive cannot run here. Fail before the download rather
    # than install a binary that will not execute.
    macos-x86_64)
      echo "Unsupported platform: macos-x86_64." >&2
      echo "Only Apple Silicon macOS builds are published; Rosetta 2 cannot run a native arm64 binary on an Intel Mac." >&2
      echo "Build from source instead: cargo install --git https://github.com/${REPO}" >&2
      exit 1
      ;;
    # Unreachable while the two cases above cover every os/arch pair, but an
    # unmatched pair would otherwise leave `target` empty and request a
    # nonsense archive URL.
    *)
      echo "Unsupported platform: ${os}-${arch}" >&2
      exit 1
      ;;
  esac

  # ── resolve latest version ─────────────────────────────────────────────────
  VERSION="${SENV_VERSION:-}"
  if [ -z "$VERSION" ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  fi

  if [ -z "$VERSION" ]; then
    echo "Could not determine latest version. Set SENV_VERSION=vX.Y.Z to override." >&2
    exit 1
  fi

  # Tags carry the v; a hand-set SENV_VERSION very often does not, and the
  # difference is otherwise a 404 with nothing in it to explain itself.
  case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
  esac

  # ── download ───────────────────────────────────────────────────────────────
  NAME="${BINARY}-${VERSION}-${target}"
  ARCHIVE="${NAME}.tar.gz"
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"

  echo "Installing senv ${VERSION} (${target}) → ${INSTALL_DIR}/${BINARY}"

  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT

  curl -fsSL "$URL" -o "${TMP}/${ARCHIVE}"

  # ── verify against the checksum the release publishes ──────────────────────
  # Not a substitute for signing — same origin as the archive — but it does
  # catch a truncated or corrupted download, and it makes tampering with the
  # asset alone insufficient. SENV_SKIP_CHECKSUM=1 is an explicit, visible
  # escape hatch rather than a silent skip.
  if [ "${SENV_SKIP_CHECKSUM:-0}" = "1" ]; then
    echo "!  checksum verification skipped (SENV_SKIP_CHECKSUM=1)" >&2
  else
    if command -v sha256sum >/dev/null 2>&1; then
      sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
    elif command -v shasum >/dev/null 2>&1; then
      sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
    else
      echo "Neither sha256sum nor shasum found; cannot verify the download." >&2
      echo "Install one, or re-run with SENV_SKIP_CHECKSUM=1 to accept the risk." >&2
      exit 1
    fi

    if ! curl -fsSL "${URL}.sha256" -o "${TMP}/${ARCHIVE}.sha256"; then
      echo "Could not fetch ${ARCHIVE}.sha256 — refusing to install unverified." >&2
      echo "Re-run with SENV_SKIP_CHECKSUM=1 to accept the risk." >&2
      exit 1
    fi

    expected="$(cut -d' ' -f1 < "${TMP}/${ARCHIVE}.sha256")"
    actual="$(sha256_of "${TMP}/${ARCHIVE}")"
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
      echo "Checksum mismatch for ${ARCHIVE}" >&2
      echo "  expected: ${expected:-<empty>}" >&2
      echo "  actual:   ${actual}" >&2
      exit 1
    fi
  fi

  tar -xzf "${TMP}/${ARCHIVE}" -C "$TMP"

  # The archive holds a versioned directory rather than a bare binary, because
  # it also carries README/LICENSE/DESIGN. Fail here rather than let `install`
  # report a missing file if that layout ever changes.
  if [ ! -f "${TMP}/${NAME}/${BINARY}" ]; then
    echo "Archive did not contain ${NAME}/${BINARY}" >&2
    exit 1
  fi

  # ── install ────────────────────────────────────────────────────────────────
  # `install` rather than `mv`: `mv` preserves the *invoking user's* ownership,
  # which under sudo leaves a user-writable senv sitting in a root-owned PATH
  # directory. Anything running as that user — including code senv is confining,
  # which shares the uid — could then replace the binary that sets up the
  # boundary before the next `senv run` ever established one.
  if [ -w "$INSTALL_DIR" ]; then
    install -m 755 "${TMP}/${NAME}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
  else
    sudo install -o root -g 0 -m 755 "${TMP}/${NAME}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
  fi

  echo "✔  senv ${VERSION} installed"

  # ── the things senv needs that this script does not install ────────────────
  # Reported after the install, not before: none of it blocks having the
  # binary, and `senv doctor` is the authority on what this machine can
  # actually enforce. These are the two cases where the answer is "nothing",
  # and it is friendlier to say so now than to let the first command fail.
  missing=""
  command -v uv >/dev/null 2>&1 || missing="uv"
  if [ "$os" = "linux" ]; then
    for tool in slirp4netns nft; do
      command -v "$tool" >/dev/null 2>&1 || missing="${missing:+$missing }$tool"
    done
  fi

  if [ -n "$missing" ]; then
    echo
    echo "Not on PATH: ${missing}"
    case "$missing" in
      *uv*)
        echo "  senv drives uv; install it from https://docs.astral.sh/uv/"
        ;;
    esac
    case "$missing" in
      *slirp4netns* | *nft*)
        echo "  Registry allowlisting during installs needs slirp4netns and nftables"
        echo "  (sudo apt install slirp4netns nftables on Debian/Ubuntu)."
        ;;
    esac
  fi

  echo
  echo "Next: senv doctor   — what this machine can enforce"
}

main "$@"
