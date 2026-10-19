-- Sample schema used for manual testing (portable SQL)
CREATE TABLE customers (
  id INTEGER PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  email VARCHAR(150),
  city VARCHAR(60),
  created_at DATE
);
CREATE TABLE products (
  id INTEGER PRIMARY KEY,
  title VARCHAR(120) NOT NULL,
  price DECIMAL(10,2) NOT NULL DEFAULT 0,
  stock INTEGER DEFAULT 0
);
CREATE TABLE orders (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  product_id INTEGER NOT NULL REFERENCES products(id),
  qty INTEGER NOT NULL DEFAULT 1,
  ordered_at DATE
);
CREATE INDEX ix_orders_customer ON orders(customer_id);
INSERT INTO customers VALUES (1,'Ayşe Yılmaz','ayse@example.com','İstanbul','2024-01-10'),(2,'John Smith','john@example.org','London','2024-02-02'),(3,'Mehmet Kaya','mehmet@example.net','Ankara','2024-03-15'),(4,'Emma Brown',NULL,'Berlin','2024-04-20');
INSERT INTO products VALUES (1,'Keyboard',49.90,120),(2,'Mouse',19.50,300),(3,'Monitor 27"',289.00,40),(4,'USB-C Cable',9.99,1000);
INSERT INTO orders VALUES (1,1,1,1,'2024-05-01'),(2,1,2,2,'2024-05-02'),(3,2,3,1,'2024-05-03'),(4,3,4,5,'2024-05-04'),(5,4,1,1,'2024-05-05'),(6,2,2,3,'2024-05-06');
CREATE VIEW order_summary AS SELECT c.name, p.title, o.qty, p.price * o.qty AS total FROM orders o JOIN customers c ON c.id = o.customer_id JOIN products p ON p.id = o.product_id;
