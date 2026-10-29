//! Sample SQLite database ("shop") that a fresh install opens with, so the
//! navigator, data grid, designers and ER model have real content before the
//! user connects to a server of their own.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::config::{ConnectionConfig, DbKind, SavedQuery};

pub const CONNECTION_NAME: &str = "Demo · Shop (SQLite)";

const FIRST: &[&str] = &[
    "Emma", "Liam", "Olivia", "Noah", "Ava", "Mateo", "Sofia", "Lucas", "Mia", "Elif", "Can", "Zeynep", "Yuki", "Lea", "Omar", "Ana",
];
const LAST: &[&str] = &[
    "Johnson", "Garcia", "Müller", "Rossi", "Silva", "Kaya", "Tanaka", "Dubois", "Novak", "Yılmaz", "Brown", "Haddad", "Costa", "Weber",
];
const CITIES: &[&str] = &["Berlin", "London", "Paris", "Madrid", "Rome", "Lisbon", "Istanbul", "Tokyo", "New York", "Toronto", "Sydney", "Amsterdam"];
const PRODUCTS: &[(&str, &str, f64)] = &[
    ("Mechanical Keyboard", "Peripherals", 129.0),
    ("Wireless Mouse", "Peripherals", 49.5),
    ("27\" 4K Monitor", "Displays", 399.0),
    ("USB-C Hub", "Accessories", 59.9),
    ("Laptop Stand", "Accessories", 39.0),
    ("Noise-Cancelling Headphones", "Audio", 279.0),
    ("Webcam 1080p", "Peripherals", 89.0),
    ("Portable SSD 1 TB", "Storage", 119.0),
    ("Desk Lamp", "Office", 45.0),
    ("Ergonomic Chair", "Office", 349.0),
    ("Standing Desk", "Office", 549.0),
    ("Bluetooth Speaker", "Audio", 99.0),
    ("Smartwatch", "Wearables", 249.0),
    ("Fitness Band", "Wearables", 79.0),
    ("Graphics Tablet", "Peripherals", 199.0),
    ("Microphone", "Audio", 149.0),
];
const STATUSES: &[&str] = &["paid", "paid", "paid", "shipped", "shipped", "pending", "cancelled"];
const TIERS: &[&str] = &["basic", "basic", "team", "enterprise"];

const SCHEMA: &str = "
CREATE TABLE customers (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    email       TEXT NOT NULL UNIQUE,
    city        TEXT,
    tier        TEXT NOT NULL DEFAULT 'basic' CHECK (tier IN ('basic', 'team', 'enterprise')),
    created_at  TEXT NOT NULL
);
CREATE TABLE products (
    id        INTEGER PRIMARY KEY,
    title     TEXT NOT NULL,
    category  TEXT NOT NULL,
    price     NUMERIC(10,2) NOT NULL,
    stock     INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE orders (
    id           INTEGER PRIMARY KEY,
    customer_id  INTEGER NOT NULL REFERENCES customers(id),
    status       TEXT NOT NULL CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled')),
    ordered_at   TEXT NOT NULL
);
CREATE TABLE order_items (
    order_id    INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    product_id  INTEGER NOT NULL REFERENCES products(id),
    qty         INTEGER NOT NULL CHECK (qty > 0),
    unit_price  NUMERIC(10,2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);
CREATE INDEX idx_orders_customer ON orders(customer_id);
CREATE INDEX idx_orders_ordered_at ON orders(ordered_at);
CREATE VIEW customer_revenue AS
    SELECT c.id, c.name, c.city, c.tier, COUNT(DISTINCT o.id) AS orders, ROUND(SUM(oi.qty * oi.unit_price), 2) AS revenue
    FROM customers c
    JOIN orders o ON o.customer_id = c.id AND o.status <> 'cancelled'
    JOIN order_items oi ON oi.order_id = o.id
    GROUP BY c.id;
CREATE TRIGGER products_stock_after_item AFTER INSERT ON order_items
BEGIN
    UPDATE products SET stock = stock - NEW.qty WHERE id = NEW.product_id;
END;
";

pub const SAMPLE_QUERY: &str = "-- Top customers by revenue
SELECT c.name, c.city, c.tier,
       COUNT(DISTINCT o.id) AS orders,
       ROUND(SUM(oi.qty * oi.unit_price), 2) AS revenue
FROM customers c
JOIN orders o ON o.customer_id = c.id
JOIN order_items oi ON oi.order_id = o.id
WHERE o.status <> 'cancelled'
GROUP BY c.id
ORDER BY revenue DESC
LIMIT 20;
";

/// Small deterministic generator, so every install gets the same sample data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Writes the sample database to `path`, replacing whatever is there.
pub fn create(path: &Path) -> crate::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut db = Connection::open(path)?;
    db.execute_batch(SCHEMA)?;
    let tx = db.transaction()?;
    let mut rng = Rng(20261029);
    for (i, (title, category, price)) in PRODUCTS.iter().enumerate() {
        tx.execute(
            "INSERT INTO products (id, title, category, price, stock) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![i as i64 + 1, title, category, price, 400 + rng.below(400) as i64],
        )?;
    }
    let customers = 80;
    for id in 1..=customers {
        let (first, last) = (FIRST[rng.below(FIRST.len())], LAST[rng.below(LAST.len())]);
        let email = format!("{}.{}{id}@example.com", first.to_lowercase(), ascii(last).to_lowercase());
        let created = format!("2025-{:02}-{:02} {:02}:{:02}:00", 1 + rng.below(12), 1 + rng.below(28), 8 + rng.below(12), rng.below(60));
        tx.execute(
            "INSERT INTO customers (id, name, email, city, tier, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, format!("{first} {last}"), email, CITIES[rng.below(CITIES.len())], TIERS[rng.below(TIERS.len())], created],
        )?;
    }
    for id in 1..=300i64 {
        let ordered = format!("2026-{:02}-{:02} {:02}:{:02}:00", 1 + rng.below(9), 1 + rng.below(28), 8 + rng.below(14), rng.below(60));
        tx.execute(
            "INSERT INTO orders (id, customer_id, status, ordered_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, 1 + rng.below(customers as usize) as i64, STATUSES[rng.below(STATUSES.len())], ordered],
        )?;
        let mut picked = Vec::new();
        for _ in 0..1 + rng.below(4) {
            let p = rng.below(PRODUCTS.len());
            if picked.contains(&p) {
                continue;
            }
            picked.push(p);
            tx.execute(
                "INSERT INTO order_items (order_id, product_id, qty, unit_price) VALUES (?1, ?2, ?3, ?4)",
                params![id, p as i64 + 1, 1 + rng.below(3) as i64, PRODUCTS[p].2],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The navigator entry for the sample database at `path`.
pub fn connection(path: &Path) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DbKind::Sqlite);
    c.name = CONNECTION_NAME.into();
    c.database = path.to_string_lossy().into_owned();
    c
}

/// A saved query on the sample database, so Queries has an entry to open.
pub fn query(conn: &ConnectionConfig) -> SavedQuery {
    SavedQuery {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Top customers".into(),
        connection_id: conn.id.clone(),
        database: "main".into(),
        sql: SAMPLE_QUERY.into(),
        modified: chrono::Local::now(),
    }
}

/// Email-safe spelling of a surname.
fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'ü' => 'u',
            'ı' => 'i',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_populated_database() {
        let dir = std::env::temp_dir().join(format!("navdb-demo-{}", std::process::id()));
        let path = dir.join("shop.sqlite");
        create(&path).unwrap();
        let db = Connection::open(&path).unwrap();
        let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM customers"), 80);
        assert_eq!(count("SELECT COUNT(*) FROM orders"), 300);
        assert!(count("SELECT COUNT(*) FROM order_items") >= 300);
        assert!(count("SELECT COUNT(*) FROM customer_revenue") > 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
