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
`NAVDB_DATA_DIR` ortam değişkeni ayar klasörünü değiştirir (varsayılan: `~/Library/Application Support/navdb-studio`).
Örnek şema: `examples/sample_shop.sql`.

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
