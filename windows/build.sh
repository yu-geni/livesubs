#!/usr/bin/env bash
# Build the Windows app (LiveSubs.exe) from WSL and place it in %LOCALAPPDATA%\LiveSubs, so that
# it never runs from a \\wsl.localhost path.
#
#   bash windows/build.sh               compile and install
#   bash windows/build.sh --no-install  compile only (to try a build before it replaces the app)
#
# Needs rustup with `rustup target add x86_64-pc-windows-gnu`, and uv: the linker is zig, which
# comes as a Python package, so there is nothing else to install (no Visual Studio, no MinGW).
set -uo pipefail
no_install=0
[[ "${1:-}" == "--no-install" ]] && no_install=1
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
distro="${WSL_DISTRO_NAME:-Ubuntu}"
port="${LIVESUBS_PORT:-8765}"
cmd=/mnt/c/Windows/System32/cmd.exe
command -v cargo >/dev/null || { echo "error: cargo not found - install Rust (https://rustup.rs) and run: rustup target add x86_64-pc-windows-gnu" >&2; exit 1; }
command -v uvx >/dev/null || { echo "error: uv not found (https://docs.astral.sh/uv/)" >&2; exit 1; }

echo "== compiling LiveSubs.exe"
# the exe's own icon, as a Windows resource (zig brings a resource compiler); build.rs links it
mkdir -p "$here/target"
(cd "$here" && uv run -q --no-project --with ziglang python -m ziglang rc /fo target/app.res app.rc >/dev/null) || echo "warning: the icon could not be compiled; the exe will have none" >&2
(cd "$here" && uvx --from cargo-zigbuild --with ziglang cargo-zigbuild build --release --target x86_64-pc-windows-gnu) || { echo "error: compilation failed" >&2; exit 1; }
out_exe="$here/target/x86_64-pc-windows-gnu/release/LiveSubs.exe"
[[ -f "$out_exe" ]] || { echo "error: $out_exe was not produced" >&2; exit 1; }
if [[ $no_install -eq 1 ]]; then
  echo "== built    : $out_exe (not installed)"
  exit 0
fi

# %LOCALAPPDATA% resolved at runtime; cmd.exe is run from /mnt/c so it gets no UNC cwd warning
local_appdata_win="$(cd /mnt/c && "$cmd" /c 'echo %LOCALAPPDATA%' | tr -d '\r')"
[[ -n "$local_appdata_win" ]] || { echo "error: could not resolve %LOCALAPPDATA%" >&2; exit 1; }
install_dir="$(wslpath -u "$local_appdata_win")/LiveSubs"
mkdir -p "$install_dir"

# Windows (Smart App Control, the virus scanner) decides per build whether an unsigned exe may
# run. Try the new build before it replaces the installed one, so that a refused build never
# takes a working app away. The try draws both views into files; no window is opened.
probe="$install_dir/LiveSubs.new.exe"
shot="$install_dir/LiveSubs.probe.raw"
shot_main="$install_dir/LiveSubs.probe-main.raw"
cp -f "$out_exe" "$probe" || { echo "error: could not write $probe" >&2; exit 1; }
rm -f "$shot" "$shot_main"
(cd /mnt/c && "$probe" --selftest-overlay-shot "$(wslpath -w "$shot")") 2>/dev/null
# the normal window is the view the app starts in, so it is tried as well
(cd /mnt/c && "$probe" --selftest-main-shot "$(wslpath -w "$shot_main")") 2>/dev/null
if [[ ! -s "$shot" || ! -s "$shot_main" ]]; then
  rm -f "$probe" "$shot" "$shot_main"
  echo >&2
  echo "error: the new build did not draw both views (Windows refused to run it? Smart App Control?)." >&2
  echo "       The installed LiveSubs.exe was left as it was; the new build is $out_exe." >&2
  exit 2
fi
rm -f "$shot" "$shot_main"
if ! mv -f "$probe" "$install_dir/LiveSubs.exe"; then
  rm -f "$probe"
  echo >&2
  echo "error: could not replace $install_dir/LiveSubs.exe" >&2
  echo "       LiveSubs.exe is probably still running - close it (tray icon -> 終了) and build again." >&2
  exit 1
fi
# left over from the app's predecessor and from the time the two were installed side by side
rm -f "$install_dir/LiveSubs.exe.config" "$install_dir/LiveSubsRs.exe" 2>/dev/null \
  || echo "note: $install_dir/LiveSubsRs.exe is still running; it is removed by the next build after it has been closed"

# Where the engine is. Other lines of an existing file (tabport=...) are kept. Written beside the
# file and moved over it: the app must never find a half-written or empty LiveSubs.ini.
ini="$install_dir/LiveSubs.ini"
kept="$(grep -v -i -E '^\s*(distro|dir|port|render|overlay)\s*=' "$ini" 2>/dev/null | tr -d '\r' | sed '/^\s*$/d')"
ini_tmp="$ini.new"
{
  printf 'distro=%s\ndir=%s\nport=%s\n' "$distro" "$repo" "$port"
  if [[ -n "$kept" ]]; then printf '%s\n' "$kept"; fi
} > "$ini_tmp" && mv -f "$ini_tmp" "$ini" || {
  rm -f "$ini_tmp"
  echo "error: could not write $ini - the app would not find the engine" >&2
  exit 1
}

# The Chrome extension is loaded from here ("パッケージ化されていない拡張機能を読み込む"), so it
# sits beside the exe on the Windows side. Copied over in place: Chrome may have it loaded.
mkdir -p "$install_dir/extension"
cp -rf "$repo/extension/." "$install_dir/extension/" || echo "warning: could not copy the Chrome extension" >&2

# The shortcut takes its picture from a copy of the icon whose name changes with its content:
# Windows keeps a shortcut's picture by path and would go on showing the old one otherwise.
icon_tag="$(sha1sum "$here/app.ico" | cut -c1-8)"
[[ -f "$install_dir/LiveSubs-$icon_tag.ico" ]] || { rm -f "$install_dir"/LiveSubs-*.ico; cp -f "$here/app.ico" "$install_dir/LiveSubs-$icon_tag.ico" || echo "warning: could not copy the icon" >&2; }

if (cd /mnt/c && "$install_dir/LiveSubs.exe" --install-shortcut) 2>/dev/null; then
  echo "== shortcut : ライブ字幕.lnk written to the Desktop"
else
  echo "== shortcut : could not be written (start the app from $local_appdata_win\\LiveSubs\\LiveSubs.exe)"
fi
echo "== installed: $install_dir/LiveSubs.exe"
echo "== config   : $ini (distro=$distro dir=$repo port=$port)"
echo "== extension: $local_appdata_win\\LiveSubs\\extension (load it in chrome://extensions, or press its reload button)"
