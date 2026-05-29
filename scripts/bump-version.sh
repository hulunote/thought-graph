#!/usr/bin/env bash
set -euo pipefail

# 用法: ./bump-version.sh 0.1.1
if [ $# -ne 1 ]; then
  echo "用法: $0 <版本号>"
  echo "示例: $0 0.1.1"
  exit 1
fi

VERSION="$1"

# 校验版本号格式 x.y.z (可带 -beta 之类后缀)
if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
  echo "错误: 版本号格式应为 x.y.z，例如 0.1.1"
  exit 1
fi

# 跨平台 sed -i (兼容 GNU sed 与 macOS/BSD sed)
sed_inplace() {
  if sed --version >/dev/null 2>&1; then
    sed -i "$@"        # GNU sed (Linux)
  else
    sed -i '' "$@"     # BSD sed (macOS)
  fi
}

# Cargo.toml: 仅替换行首的 version = "..."（避免动到依赖里的 version）
update_toml() {
  local f="$1"
  if [ -f "$f" ]; then
    sed_inplace -E "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$f"
    echo "✓ 已更新 $f"
  else
    echo "· 跳过(不存在) $f"
  fi
}

# JSON: 替换 "version": "..."
update_json() {
  local f="$1"
  if [ -f "$f" ]; then
    sed_inplace -E "s/(\"version\"[[:space:]]*:[[:space:]]*\")[^\"]*\"/\1$VERSION\"/" "$f"
    echo "✓ 已更新 $f"
  else
    echo "· 跳过(不存在) $f"
  fi
}

update_toml "Cargo.toml"
update_toml "src-tauri/Cargo.toml"
update_json "package.json"
update_json "src-tauri/tauri.conf.json"

echo "全部更新为版本 $VERSION"
