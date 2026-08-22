#!/usr/bin/env bash
# Benennt das Projekt von MediaVault in einen neuen Namen um.
# Aufruf:  ./rename.sh NeuerName
# Beispiel: ./rename.sh Fero
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "Aufruf: $0 NeuerName (z.B. Fero)" >&2
  exit 1
fi

NEW="$1"
LOWER="$(printf '%s' "$NEW" | tr '[:upper:]' '[:lower:]')"
UPPER="$(printf '%s' "$NEW" | tr '[:lower:]' '[:upper:]')"

if ! printf '%s' "$NEW" | grep -Eq '^[A-Za-z][A-Za-z0-9]*$'; then
  echo "Bitte nur Buchstaben/Ziffern ohne Leerzeichen verwenden." >&2
  exit 1
fi

cd "$(dirname "$0")"

# Alle relevanten Textdateien (inkl. Cargo.lock, dort steht der Paketname).
FILES=$(git ls-files | grep -E '\.(rs|toml|json|html|js|css|yml|md)$|^Cargo\.lock$' || true)

for f in $FILES; do
  # Reihenfolge: längste/spezifischste Muster zuerst.
  sed -i.bak \
    -e "s/Media Vault/${NEW}/g" \
    -e "s/MediaVault/${NEW}/g" \
    -e "s/MEDIAVAULT/${UPPER}/g" \
    -e "s/mediavault/${LOWER}/g" \
    "$f"
  rm -f "$f.bak"
done

echo "Ersetzt: MediaVault -> ${NEW}, mediavault -> ${LOWER}."
echo
echo "Hinweise:"
echo "  - Konfig-Ordner heisst jetzt ~/.${LOWER} (statt ~/.mediavault)."
echo "    Abos/Sessions uebernehmen:  cp -r ~/.mediavault ~/.${LOWER}"
echo "  - Tauri-Identifier ist jetzt dev.fenron.${LOWER}."
echo "  - localStorage-Schluessel beginnen jetzt mit '${LOWER}.' - alte"
echo "    UI-Einstellungen im Webview gehen verloren (nur Kosmetik)."
echo "  - Pruefen:  cargo check --locked && cargo test"
echo "  - Danach:   git add -A && git commit -m 'chore: Projekt in ${NEW} umbenannt'"
