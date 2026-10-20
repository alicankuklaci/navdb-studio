# Mac App Store release

The App Store edition is built from the same source with
`--no-default-features --features appstore`:

| | GitHub release | Mac App Store |
|---|---|---|
| App Sandbox | no | yes (`packaging/macos/appstore.entitlements`) |
| Oracle | yes | no (Instant Client cannot be loaded in the sandbox) |
| SSH tunnels | yes | no (the system `ssh` client cannot reach `~/.ssh`) |
| Pro features | all free | subscription (StoreKit 2, `crates/navdb-app/macos/NavDBBridge.swift`) |
| File access | any path | files picked in dialogs, kept with security-scoped bookmarks |

Free edition: up to `store::FREE_CONNECTIONS` (2) saved connections with the
editor, grid and designers. Pro: unlimited connections, connection import, Data
Transfer, Structure/Data Sync, ER model, Charts, Automation (`store::require_pro`
call sites in `commands.rs`).

## One-time setup

1. **Bundle ID** `dev.navdb.studio` (Certificates, Identifiers & Profiles → Identifiers, platform macOS).
2. **Certificates** in the login keychain: *Apple Distribution* and *Mac Installer Distribution*.
3. **Provisioning profile**: type *Mac App Store Connect* for `dev.navdb.studio`; save it as
   `packaging/macos/appstore.provisionprofile` (git-ignored).
4. **App record** in App Store Connect → Apps → + → New App: platform macOS, name *NavDB Studio*,
   bundle ID `dev.navdb.studio`, SKU `navdb-studio`.
5. **Subscriptions** (App → Monetization → Subscriptions): one group *NavDB Studio Pro* with
   - `dev.navdb.studio.pro.monthly`, 1 month
   - `dev.navdb.studio.pro.yearly`, 1 year

   Optional introductory offer: free trial (e.g. 14 days). Each needs a display name, description and
   a review screenshot of the Pro window (*Help → NavDB Studio Pro…*).
6. **Paid Applications agreement**, banking and tax forms (Business section) — required before
   subscriptions can be sold.
7. **App Privacy**: "Data Not Collected". Privacy policy URL:
   `https://github.com/alicankuklaci/navdb-studio/blob/main/docs/PRIVACY.md`.
   For subscriptions the description must also link the Terms of Use (Apple standard EULA).

`scripts/asc.py signing` does steps 1–3 and `scripts/asc.py subscriptions` step 5 through the
App Store Connect API (steps 4 and 6 need the web UI).

### Pricing

| Product | Price (US) | Trial |
|---|---|---|
| Pro Monthly | $6.99 | 14 days free |
| Pro Yearly | $49.99 (≈ $4.17/month, 40 % off) | 14 days free |

Chosen against the market in October 2026: DataGrip ≈ $19/month (yearly billing), SQLPro Studio
$15.99 / $99.99 in-app, Beekeeper Studio $9–14/month, TablePlus $99 one-time, Navicat Premium
subscription with a 14-day trial. NavDB Studio sits below the paid alternatives with a generous
free tier; other storefronts use Apple's equalized prices.

## Each release

```bash
BUILD_NUMBER=$(date -u +%Y%m%d%H%M) scripts/build-appstore.sh            # → dist/appstore/NavDB-Studio-appstore.pkg
ASC_KEY_ID=… ASC_ISSUER_ID=… scripts/build-appstore.sh --upload          # build and upload
```

The upload appears under TestFlight after processing; then pick it in the App Store version
page and submit for review.

## Store listing draft

- **Name:** NavDB Studio
- **Subtitle:** SQL & NoSQL database client
- **Category:** Developer Tools (secondary: Productivity)
- **Keywords:** sql,database,mysql,postgresql,sqlite,mongodb,redis,clickhouse,duckdb,query,gui,client
- **Promotional text:** One fast, native app for 30 database engines.
- **Description:** NavDB Studio is a fast, native database client for MySQL, MariaDB, PostgreSQL,
  SQLite, SQL Server, MongoDB, Redis, ClickHouse, DuckDB, Cassandra, Elasticsearch and 20 more
  engines. Browse and edit data in a spreadsheet-style grid, write SQL with context-aware
  autocomplete, design tables with a live SQL preview, and import or export CSV, JSON, Excel and SQL.
  NavDB Studio Pro adds unlimited connections, data transfer between engines, structure and data
  sync, ER diagrams, charts and scheduled automation.
