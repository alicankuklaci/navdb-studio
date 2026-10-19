# NavDB Studio

Navicat Premium'un özellik setini temel alan, Rust ile (egui/eframe) sıfırdan yazılmış çoklu veritabanı yönetim aracı.
Referans uygulamadan kod veya görsel kopyalanmadı; yalnızca ekran/özellik envanteri çıkarıldı (`docs/reference-screen-inventory.txt`).

## Desteklenen veritabanları
MySQL · MariaDB · PostgreSQL · SQLite · Redis (SSH tüneli ve SSL dahil)

## Çalıştırma
```bash
cargo run --release -p navdb-app        # binary: target/release/navdb
cargo test --workspace                  # unit testler
NAVDB_LIVE=1 cargo test -p navdb-core --test live -- --test-threads=1   # canlı sunucu testleri
```
`NAVDB_DATA_DIR` ortam değişkeni ayar klasörünü değiştirir (varsayılan: macOS `~/Library/Application Support/navdb-studio`, Windows `%APPDATA%\navdb-studio`, Linux `~/.config/navdb-studio`).
Örnek şema: `examples/sample_shop.sql`.

## Building

Rust stable (via [rustup](https://rustup.rs)) on all platforms, then `cargo build --release -p navdb-app`.

| OS | Prerequisites |
|---|---|
| Windows (x86_64) | Visual Studio Build Tools 2019+ with the "Desktop development with C++" workload (MSVC + Windows SDK). |
| macOS (arm64 / x86_64) | Xcode Command Line Tools (`xcode-select --install`). `scripts/bundle-macos.sh target/release/navdb dist` builds `NavDB Studio.app`. |
| Linux (x86_64) | `build-essential pkg-config libssl-dev libxkbcommon-dev libwayland-dev` (Debian/Ubuntu names). At runtime: an OpenGL driver, X11 or Wayland, and `xdg-desktop-portal` for file dialogs. `scripts/package-linux.sh` builds a `.tar.gz` and `.deb`. |

Notes:
- **Oracle** connections need [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) at runtime on every OS (the driver loads the OCI library dynamically; put it on `PATH` / `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH`). The app builds and runs without it; only Oracle connections fail.
- SSH tunnels use the system `ssh` client (OpenSSH; on Windows 10+ the built-in "OpenSSH Client" feature).
- Release builds for all platforms are produced by `.github/workflows/release.yml` when a `v*` tag is pushed. The macOS app is only ad-hoc signed: on first launch use right-click → Open, or `xattr -dr com.apple.quarantine "NavDB Studio.app"`.

## Ekranlar
- Ana pencere: menü, araç çubuğu, navigator ağacı (bağlantı → veritabanı → şema → nesneler, sağ tık menüleri), Objects listesi, bilgi/DDL paneli
- Bağlantı formu (General / Advanced / Databases / SSL / SSH), bağlantı import/export, gruplar, renkler
- Tablo görüntüleyici: sayfalama, düzenleme, ekle/sil, Apply/Discard (transaction), filtre oluşturucu, WHERE, sıralama, Form görünümü, hücre editörü
- Sorgu editörü: sözdizimi renklendirme, otomatik tamamlama, çoklu sonuç, Explain, Beautify, snippet, kayıtlı sorgular
- Tablo / View / Fonksiyon-Prosedür-Trigger-Event-Sequence-Index tasarımcıları (SQL önizleme)
- Import / Export sihirbazları (CSV, TSV, TXT, JSON, XML, HTML, Markdown, SQL, Excel XML)
- Dump SQL, Execute SQL File, Backup/Restore
- Data Transfer, Structure Sync, Data Sync, Data Generation
- Server Monitor, Users, Console, History Log, Find in Database, Automation
- ER Model (reverse engineering, SVG/SQL export), Charts/Dashboard, Redis key editörü, Preferences

Mimari: `docs/ARCHITECTURE.md`.
