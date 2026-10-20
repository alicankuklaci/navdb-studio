<div align="center">

<img src="crates/navdb-app/assets/icon/navdb-256.png" width="112" alt="NavDB Studio logosu">

# NavDB Studio

**31 veritabanı motoru için hızlı, yerel (native) bir veritabanı arayüzü — ücretsiz ve açık kaynak.**

Navicat tarzı çalışma düzeni, Rust ile sıfırdan yazıldı. MySQL, PostgreSQL, SQLite, SQL Server, Oracle, MongoDB, Redis, ClickHouse, DuckDB, Cassandra, Elasticsearch ve daha fazlası için tek uygulama.

[![CI](https://github.com/alicankuklaci/navdb-studio/actions/workflows/ci.yml/badge.svg)](https://github.com/alicankuklaci/navdb-studio/actions/workflows/ci.yml)
[![Son sürüm](https://img.shields.io/github/v/release/alicankuklaci/navdb-studio?sort=semver)](https://github.com/alicankuklaci/navdb-studio/releases/latest)
[![İndirmeler](https://img.shields.io/github/downloads/alicankuklaci/navdb-studio/total)](https://github.com/alicankuklaci/navdb-studio/releases)
[![Lisans: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#lisans)
![Platformlar](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)

[English](README.md) · **Türkçe**

[**İndir**](#indir) · [Özellikler](#özellikler) · [Desteklenen veritabanları](#desteklenen-veritabanları) · [Kaynaktan derleme](#kaynaktan-derleme) · [Katkıda bulunma](#katkıda-bulunma)

<img src="docs/screenshots/main.png" alt="NavDB Studio ana pencere" width="900">

</div>

---

## Neden NavDB Studio?

- **Tek araç, 31 motor.** İlişkisel, analitik, doküman, anahtar-değer, arama ve geniş sütunlu veritabanları aynı gezgini, aynı SQL editörünü ve aynı veri tablosunu kullanır.
- **Yerel ve hafif.** Rust ve [egui](https://github.com/emilk/egui) ile yazılmış tek, bağımsız bir program. Electron yok, JVM yok, kurulacak ek çalışma ortamı yok.
- **Navicat tarzı, üstüne daha fazlası.** Tablo tasarımcısı, veri aktarımı, yapı ve veri senkronizasyonu, ER diyagramları, grafikler ve otomasyon; ayrıca bağlama duyarlı otomatik tamamlama, DuckDB ve lakehouse motorları gibi ekstralar.
- **Ücretsiz ve açık kaynak.** Lisans anahtarı yok, deneme süresi yok, telemetri yok. MIT / Apache-2.0 çift lisanslı.
- **Çapraz platform.** Windows, macOS (Apple Silicon ve Intel) ve Linux (x64 ve arm64).

## İndir

Her sürümün derlenmiş dosyaları [**Releases sayfasında**](https://github.com/alicankuklaci/navdb-studio/releases/latest).

| Platform | İndir | Not |
|---|---|---|
| **Windows** 10/11 (x64) | [NavDB-Studio-windows-x64.zip](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-windows-x64.zip) | Zip'i açıp `NavDB Studio.exe` dosyasını çalıştırın |
| **macOS** Apple Silicon | [NavDB-Studio-macos-arm64.dmg](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-macos-arm64.dmg) | macOS 11 veya üstü |
| **macOS** Intel | [NavDB-Studio-macos-x64.dmg](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-macos-x64.dmg) | macOS 11 veya üstü |
| **Linux** x64 — Debian/Ubuntu | [NavDB-Studio-linux-x64.deb](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-linux-x64.deb) | `sudo apt install ./NavDB-Studio-linux-x64.deb` |
| **Linux** x64 — tüm dağıtımlar | [NavDB-Studio-linux-x64.tar.gz](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-linux-x64.tar.gz) | glibc 2.35+ (Ubuntu 22.04, Debian 12, Fedora 36 veya daha yenisi) |
| **Linux** arm64 | [.deb](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-linux-arm64.deb) · [.tar.gz](https://github.com/alicankuklaci/navdb-studio/releases/latest/download/NavDB-Studio-linux-arm64.tar.gz) | Raspberry Pi 4/5, ARM sunucular |

Her sürümde indirmeleri doğrulamak için bir `SHA256SUMS.txt` dosyası da bulunur.

> [!IMPORTANT]
> **macOS'ta ilk açılış:** Uygulama henüz Apple tarafından onaylanmadığı (notarize edilmediği) için macOS *"Apple could not verify 'NavDB Studio' is free of malware…"* uyarısı gösterir. Bu beklenen bir durumdur; dosyalar bu depodan [GitHub Actions](https://github.com/alicankuklaci/navdb-studio/actions/workflows/release.yml) ile derlenir. Bir kereliğine açmak için:
>
> 1. `.dmg` içindeki *NavDB Studio*'yu *Uygulamalar* (Applications) klasörüne sürükleyip açmayı deneyin. Uyarıda **Bitti** (Done) düğmesine basın.
> 2. **Sistem Ayarları → Gizlilik ve Güvenlik** bölümünü açın, aşağıdaki *"NavDB Studio" engellendi…* satırını bulun ve **Yine de Aç** (Open Anyway) düğmesine basın.
>
> Ya da Terminal'de bir kez şunu çalıştırın: `xattr -dr com.apple.quarantine "/Applications/NavDB Studio.app"`
>
> **Windows:** SmartScreen penceresinde **Ek bilgi → Yine de çalıştır** seçin. **Linux:** ek bir adım gerekmez.

## Özellikler

<table>
<tr>
<td width="50%" valign="top">

### Verileri keşfedin ve düzenleyin
- Gezgin ağacı: bağlantılar → veritabanları → şemalar → tablolar, görünümler, fonksiyonlar, prosedürler, tetikleyiciler, olaylar, sequence'ler, indeksler
- Sayfalama, satır içi düzenleme, ekleme ve silme; işlem (transaction) içinde **Uygula / Vazgeç**
- Görsel filtre oluşturucu, ham `WHERE`, çoklu sütun sıralama, form görünümü, uzun metin ve JSON için hücre editörü
- Bağlantı grupları ve renkleri, salt okunur bağlantılar, SSH tünelleri, SSL/TLS

</td>
<td width="50%" valign="top">

### SQL editörü
- Sözdizimi renklendirme, birden çok sonuç sekmesi, `EXPLAIN`, biçimlendirme ve sıkıştırma
- **Bağlama duyarlı otomatik tamamlama:** `FROM`/`JOIN` sonrası tablolar, takma adların sütunları (`o.` → `orders` sütunları), CTE ve alt sorgu sütunları, veritabanı ve şema önekleri, imzalarıyla birlikte lehçeye özel fonksiyonlar
- **JOIN koşulu önerisi:** `ON` yazınca yabancı anahtarlardan (yoksa `user_id = id` adlandırmasından) hazır koşul
- **Fonksiyon imza ipucu:** parantez içinde yazılan parametre vurgulanır
- **Üzerine gelince bilgi:** tablonun sütunları, sütunun tipi ve başvurduğu anahtar, fonksiyon imzası
- Kod parçacıkları (snippet), kayıtlı sorgular, sorgu geçmişi
- İmlecin bulunduğu ifadeyi, seçimi ya da bütün bir SQL dosyasını çalıştırma

</td>
</tr>
<tr>
<td valign="top">

### Tasarım
- Tablo tasarımcısı: sütunlar, indeksler, yabancı anahtarlar, check kısıtları ve tetikleyiciler; canlı **SQL önizleme**
- Görünüm, fonksiyon, prosedür, tetikleyici, olay ve sequence tasarımcıları
- **ER modeli:** şemadan diyagram çıkarma ve SVG ya da SQL olarak dışa aktarma
- Kullanıcı ve yetki yönetimi

</td>
<td valign="top">

### Veri taşıma ve karşılaştırma
- İçe aktarma: CSV, TSV, JSON, XML
- Dışa aktarma: CSV, TSV, TXT, JSON, XML, HTML, Markdown, SQL, Excel
- Bağlantılar arası **Veri Aktarımı**, farklı motorlar arasında bile
- İncelenebilir betikle **Yapı Senkronizasyonu** ve **Veri Senkronizasyonu**
- SQL dökümü, yedekleme ve geri yükleme, test verisi üretimi

</td>
</tr>
<tr>
<td valign="top">

### İzleme ve otomasyon
- Sunucu izleme: işlemler, değişkenler, durum
- Konsol, *Veritabanında Bul*
- **Otomasyon:** sorguları, yedeklemeleri ve dışa aktarmaları belirli aralıklarla çalıştırma

</td>
<td valign="top">

### NoSQL ve analitik
- Redis anahtar tarayıcısı ve editörü (string, hash, list, set, sorted set, TTL)
- MongoDB koleksiyonları ve dokümanları
- Sorgu sonuçlarından grafikler ve panolar
- Analitik iş yükleri için DuckDB, ClickHouse, Trino/Presto ve Iceberg

</td>
</tr>
</table>

<div align="center">
<img src="docs/screenshots/autocomplete.png" alt="Bağlama duyarlı SQL otomatik tamamlama" width="780">
</div>

## Desteklenen veritabanları

<div align="center">
<img src="docs/screenshots/welcome.png" alt="Karşılama ekranında desteklenen tüm motorlar" width="780">
</div>

| Aile | Motorlar |
|---|---|
| **MySQL protokolü** | MySQL · MariaDB · TiDB · SingleStore · OceanBase · StarRocks · Apache Doris |
| **PostgreSQL protokolü** | PostgreSQL · CockroachDB · YugabyteDB · TimescaleDB · Amazon Redshift · Greenplum |
| **Diğer ilişkisel** | SQLite · Microsoft SQL Server · Oracle¹ · Vertica |
| **Analitik / lakehouse** | DuckDB · ClickHouse · Trino · Presto · Apache Iceberg |
| **Doküman** | MongoDB |
| **Anahtar-değer** | Redis · Valkey · KeyDB · Dragonfly |
| **Arama** | Elasticsearch · OpenSearch |
| **Geniş sütunlu** | Apache Cassandra · ScyllaDB |

¹ Oracle bağlantıları için [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) gerekir: Windows'ta `PATH`, macOS'ta `DYLD_LIBRARY_PATH`, Linux'ta `LD_LIBRARY_PATH` içinde olmalıdır. Uygulamanın geri kalanı onsuz da çalışır.

## Kaynaktan derleme

[rustup.rs](https://rustup.rs) üzerinden kararlı (stable) Rust araç zinciri gerekir.

```bash
git clone https://github.com/alicankuklaci/navdb-studio.git
cd navdb-studio
cargo run --release -p navdb-app        # program: target/release/navdb
```

| İşletim sistemi | Ek gereksinimler |
|---|---|
| Windows | *Desktop development with C++* bileşeniyle Visual Studio Build Tools 2019+ |
| macOS | Xcode Command Line Tools (`xcode-select --install`) |
| Linux | `build-essential pkg-config libssl-dev libxkbcommon-dev libwayland-dev libgtk-3-dev` (Debian/Ubuntu paket adları) |

Paketleme betikleri:

```bash
scripts/bundle-macos.sh target/release/navdb dist          # → dist/NavDB Studio.app
scripts/package-linux.sh target/release/navdb dist 0.1.0   # → .tar.gz + .deb
```

`v*` biçiminde bir etiket (tag) gönderildiğinde [`.github/workflows/release.yml`](.github/workflows/release.yml) çalışır, tüm platformları derler ve bir GitHub Release yayınlar.

### Testler

```bash
cargo test --workspace                                               # birim testleri
NAVDB_LIVE=1 cargo test -p navdb-core --test live -- --test-threads=1   # canlı sunuculara karşı
```

### Yapılandırma

Bağlantılar, tercihler, snippet'ler ve geçmiş şu klasörde tutulur:

| İşletim sistemi | Konum |
|---|---|
| Windows | `%APPDATA%\navdb-studio` |
| macOS | `~/Library/Application Support/navdb-studio` |
| Linux | `~/.config/navdb-studio` |

Farklı bir klasör kullanmak için (örneğin taşınabilir kurulum ya da ayrı bir test profili) `NAVDB_DATA_DIR` değişkenini ayarlayın. Parolalar yalnızca *Parolayı kaydet* seçiliyse saklanır; Unix'te dosya yalnızca sahibinin okuyabileceği izinlerle oluşturulur.

## Mimari

```
crates/navdb-core   Arayüzden bağımsız kütüphane: sürücüler, lehçeler, SQL araçları, içe/dışa aktarma, senkronizasyon (birim testli)
crates/navdb-app    egui/eframe masaüstü uygulaması (program: navdb)
```

Her bağlantı kendi iş parçacığında çalışır; arayüz ağ yüzünden asla donmaz. Ayrıntılar için [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Katkıda bulunma

Hata bildirimleri, özellik istekleri ve pull request'ler memnuniyetle karşılanır.

1. Hatayı ya da özelliği anlatan bir [issue](https://github.com/alicankuklaci/navdb-studio/issues) açın.
2. Depoyu fork'layıp bir dal (branch) oluşturun.
3. Pull request açmadan önce `cargo test --workspace` ve `cargo clippy --workspace` çalıştırın.

Yeni veritabanı sürücüleri `crates/navdb-core/src/driver/` içindeki `Driver` trait'ini uygular. Mevcut sürücüler iyi birer şablondur.

## Lisans

Aşağıdakilerden biri altında lisanslanmıştır:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT lisansı ([LICENSE-MIT](LICENSE-MIT))

Hangisini seçeceğiniz size kalmıştır. Aksini açıkça belirtmediğiniz sürece, projeye dahil edilmek üzere gönderdiğiniz her katkı ek koşul olmaksızın yukarıdaki gibi çift lisanslanır.

---

<sub>NavDB Studio bağımsız bir projedir. PremiumSoft CyberTech Ltd. ile bağlantılı değildir, onun tarafından desteklenmez veya sponsor edilmez. "Navicat" ve diğer tüm ürün adları sahiplerinin ticari markalarıdır ve burada yalnızca tanımlama amacıyla kullanılmıştır.</sub>
