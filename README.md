# Fero

Fero ist eine Tauri-Desktop-App, die Webnovels und Manga abonniert, neue
Kapitel lädt und sie als EPUB beziehungsweise CBZ in frei wählbare Zielordner
ausliefert. Abos, Einstellungen, Sitzungen und Kapitelcache liegen getrennt
von den fertigen Dateien im Fero-Datenordner.

## Installation auf macOS

1. DMG öffnen.
2. `Fero.app` in den Ordner **Programme** ziehen.
3. Das DMG auswerfen und Fero ausschließlich aus **Programme** starten.

Fero nicht direkt aus dem DMG oder aus `Downloads` öffnen. Gatekeeper startet
eine solche App gegebenenfalls aus einer zufälligen `AppTranslocation`. Bei
ad-hoc signierten Test-Builds bindet macOS Datei- und Netzwerkvolume-Freigaben
zusätzlich an den Hash genau dieses Builds. Das kann dazu führen, dass eine
Freigabe nach einem Update erneut erteilt werden muss. Ein laufender Build
sollte die Abfrage durch die serialisierten Pfadprüfungen jedoch nicht mehr als
Kaskade anzeigen.

Wenn eine alte Installation weiterhin fragt: alle Fero-Instanzen beenden, die
alte App entfernen, den aktuellen Build nach `Programme` kopieren und von dort
neu starten. Dauerhaft stabile Freigaben über mehrere Releases setzen eine mit
Developer ID signierte und notarisierte App voraus.

## Entwicklung

Voraussetzung ist eine stabile Rust-Toolchain mit `rustfmt` und `clippy`.
Das Frontend liegt ohne Bundler in `dist/` und wird beim Kompilieren direkt in
die Anwendung eingebettet.

```sh
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

Ein macOS-Bundle wird mit der Tauri-CLI gebaut:

```sh
cargo tauri build --target universal-apple-darwin
```

Die CI erlaubt ad-hoc signierte Entwicklungsartefakte. Ein Tag `v*` wird nur
noch veröffentlicht, wenn mindestens das Apple-Zertifikat vorhanden ist. Die
Secrets `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` und `APPLE_TEAM_ID`
sollten für signierte und notarisierte Releases vollständig eingerichtet sein.

## Architektur in Kürze

- `src/api/`: Quellenadapter und der gemeinsame, gedrosselte HTTP-Client.
- `src/core/`: persistente Abo-Modelle sowie EPUB-/CBZ-Erzeugung.
- `src/deliver/`: Datenordner, Zielauflösung, Manifest und Verschieben.
- `src/desktop/`: Tauri-Fenster, Custom-Protocol-API, Browserfenster und Tray.
- `dist/`: eingebettetes HTML, CSS und JavaScript.
- `Info.plist`: macOS-Zweckangaben für geschützte Ordner und Volumes.
- `.github/workflows/ci.yml`: Formatierung, Tests, Audit und Plattform-Builds.

Der aktuelle technische Prüfbericht mit offenen Punkten steht in
[`docs/REVIEW_2026-09-06.md`](docs/REVIEW_2026-09-06.md).
