#!/bin/sh
# Печатает раздел версии из CHANGELOG.md (для описания релиза на GitHub).
# Использование: scripts/changelog-section.sh 0.2.0 [CHANGELOG.md]
# Код выхода 1, если раздела нет или он пустой.
set -eu
v="${1#v}"
file="${2:-CHANGELOG.md}"
out=$(awk -v v="$v" '
  index($0, "## [" v "]") == 1 { on = 1; next }
  on && (/^## \[/ || /^\[[^]]+\]: /) { exit }
  on { print }
' "$file" | sed -e '/./,$!d')
[ -n "$(printf '%s' "$out" | tr -d '[:space:]')" ] || { echo "CHANGELOG.md: нет раздела ## [$v]" >&2; exit 1; }
printf '%s\n' "$out" | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}'
link=$(grep -m1 "^\[$v\]: " "$file" | cut -d' ' -f2- || true)
[ -z "$link" ] || printf '\n**Все изменения**: %s\n' "$link"
