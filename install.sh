#!/bin/sh
# Установка / обновление pechka (macOS, Linux):
#   curl -fsSL https://github.com/VeeB0/pechka/releases/latest/download/install.sh | sh
# Переменные: PECHKA_VERSION=v0.7.0 — конкретная версия; PECHKA_INSTALL_DIR — куда ставить (по умолчанию ~/.local/bin).
# Прежние MCTUI_VERSION / MCTUI_INSTALL_DIR / MCTUI_REPO тоже понимаются.
# Настройки (~/.config/pechka/servers.toml) установка не трогает.
set -eu

REPO="${PECHKA_REPO:-${MCTUI_REPO:-VeeB0/pechka}}"
DIR="${PECHKA_INSTALL_DIR:-${MCTUI_INSTALL_DIR:-$HOME/.local/bin}}"
VERSION="${PECHKA_VERSION:-${MCTUI_VERSION:-}}"

say() { printf '\033[1;32m›\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mошибка:\033[0m %s\n' "$*" >&2; exit 1; }

os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64)                 target=aarch64-apple-darwin ;;
  Darwin/x86_64)                target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64)     target=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64)    target=aarch64-unknown-linux-musl ;;
  *) die "нет готовой сборки для $os/$arch — соберите из исходников: cargo install --git https://github.com/$REPO" ;;
esac
# Rosetta: терминал под x86_64 на Apple Silicon — всё равно ставим нативную сборку
if [ "$target" = x86_64-apple-darwin ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  target=aarch64-apple-darwin
fi

if [ -n "$VERSION" ]; then
  base="https://github.com/$REPO/releases/download/$VERSION"
else
  base="https://github.com/$REPO/releases/latest/download"
fi
asset="pechka-$target"

command -v curl >/dev/null 2>&1 || die "нужен curl"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT INT TERM

say "Скачиваю $asset…"
curl -fSL --progress-bar "$base/$asset" -o "$tmp/pechka" || die "не удалось скачать $base/$asset"
curl -fsSL "$base/$asset.sha256" -o "$tmp/sum" || die "не удалось скачать контрольную сумму"

expected=$(cut -d' ' -f1 < "$tmp/sum")
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/pechka" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/pechka" | cut -d' ' -f1); fi
[ "$expected" = "$actual" ] || die "контрольная сумма не совпала — файл повреждён"

mkdir -p "$DIR"
chmod +x "$tmp/pechka"
# macOS: файл, скачанный curl'ом, не помечается карантином, но на всякий случай
xattr -d com.apple.quarantine "$tmp/pechka" 2>/dev/null || true
mv -f "$tmp/pechka" "$DIR/pechka"
say "Установлено: $DIR/pechka ($("$DIR/pechka" --version))"
# Программа раньше называлась mctui: старый файл заменяем ссылкой, команда mctui продолжит работать.
if [ -f "$DIR/mctui" ] && [ ! -L "$DIR/mctui" ]; then
  ln -sf pechka "$DIR/mctui"
  say "mctui теперь называется pechka; команда mctui оставлена синонимом."
fi

# PATH прописываем в файл запуска оболочки всегда (идемпотентно): текущий $PATH может прийти
# из IDE/агента и не совпадать с тем, что увидит новое окно терминала.
line="export PATH=\"$DIR:\$PATH\""
case "${SHELL:-}" in
  */zsh)  rc="$HOME/.zshrc" ;;
  */bash) if [ "$os" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
  */fish) rc="" ; mkdir -p "$HOME/.config/fish/conf.d"
          echo "fish_add_path $DIR" > "$HOME/.config/fish/conf.d/pechka.fish" ;;
  *)      rc="$HOME/.profile" ;;
esac
short_dir=$(printf '%s' "$DIR" | sed "s|^$HOME|\$HOME|")
if [ -n "$rc" ] && ! grep -qsF "$DIR" "$rc" && ! grep -qsF "$short_dir" "$rc"; then
  printf '\n# pechka\n%s\n' "$line" >> "$rc"
  say "Добавил $DIR в PATH ($rc)."
fi
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "Откройте новое окно терминала (или выполните: $line)" ;;
esac
say "Запуск: pechka    Обновление: pechka update"
