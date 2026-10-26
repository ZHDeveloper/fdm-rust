#!/usr/bin/env bash
set -euo pipefail

VERSION="3.1.0"
ARCH="macos-universal"
EXPECTED_SHA256="859D445E3852DC4E296408A8ABB62ACE48B33EC300100B90B6AF7ACC52E506BF"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${REPO_ROOT}/src-tauri/resources/amule/${ARCH}"

if [[ -x "${OUT_DIR}/bin/amuled" && -x "${OUT_DIR}/bin/amuleapi" ]]; then
  echo "aMule already packaged at ${OUT_DIR}"
  exit 0
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fdm-amule-macos.XXXXXX")"
MOUNT_DIR="${TMP_DIR}/mount"
DMG_PATH="${TMP_DIR}/aMule-${VERSION}-macOS-universal2.dmg"
mkdir -p "${MOUNT_DIR}"

cleanup() {
  hdiutil detach "${MOUNT_DIR}" -force -quiet >/dev/null 2>&1 || true
  rm -rf "${TMP_DIR}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

URL="https://github.com/amule-org/amule/releases/download/${VERSION}/aMule-${VERSION}-macOS-universal2.dmg"
echo "downloading ${URL}"
curl -fL --retry 3 --retry-delay 2 -A "FDM-packager" "${URL}" -o "${DMG_PATH}"

ACTUAL_SHA256="$(shasum -a 256 "${DMG_PATH}" | awk '{print toupper($1)}')"
if [[ "${ACTUAL_SHA256}" != "${EXPECTED_SHA256}" ]]; then
  echo "aMule DMG SHA256 mismatch: ${ACTUAL_SHA256} != ${EXPECTED_SHA256}" >&2
  exit 1
fi

hdiutil attach "${DMG_PATH}" -nobrowse -readonly -mountpoint "${MOUNT_DIR}" -quiet
APP_DIR="$(find "${MOUNT_DIR}" -maxdepth 2 -type d -name 'aMule.app' | head -n 1)"
if [[ -z "${APP_DIR}" ]]; then
  echo "aMule.app not found in DMG" >&2
  exit 1
fi

for binary in amuled amuleapi; do
  if [[ ! -x "${APP_DIR}/Contents/MacOS/${binary}" ]]; then
    echo "missing ${binary} in aMule.app" >&2
    exit 1
  fi
done

rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}/bin" "${OUT_DIR}/libs"
ditto "${APP_DIR}/Contents/MacOS" "${OUT_DIR}/bin"
if [[ -d "${APP_DIR}/Contents/libs" ]]; then
  ditto "${APP_DIR}/Contents/libs" "${OUT_DIR}/libs"
fi
if [[ -d "${APP_DIR}/Contents/Resources" ]]; then
  ditto "${APP_DIR}/Contents/Resources" "${OUT_DIR}/Resources"
fi

LICENSE="$(find "${APP_DIR}" -type f \( -name 'LICENSE.md' -o -name 'LICENSE' \) | head -n 1 || true)"
if [[ -n "${LICENSE}" ]]; then
  cp -f "${LICENSE}" "${OUT_DIR}/LICENSE.md"
fi

echo "packaged aMule into ${OUT_DIR}"
du -sh "${OUT_DIR}"
