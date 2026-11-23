# Captured plans

Real output of MySQL 8.4 and MariaDB 11.8 (`docker compose` images), for the parser tests in `tests/plans.rs`. Do not edit: recapture instead.

- `<engine>_<query>.json`: `EXPLAIN FORMAT=JSON`
- `mysql_<query>.analyze.txt`: `EXPLAIN ANALYZE` (MySQL's tree)
- `mariadb_<query>.analyze.json`: `ANALYZE FORMAT=JSON`

Measured plans ran inside `START TRANSACTION … ROLLBACK`.

## Schema

```sql
DROP DATABASE IF EXISTS plantest;
CREATE DATABASE plantest;
USE plantest;
CREATE TABLE customers (id INT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(100), country CHAR(2), KEY (country));
CREATE TABLE orders (id INT PRIMARY KEY AUTO_INCREMENT, customer_id INT, status VARCHAR(10), total DECIMAL(10,2), created_at DATETIME, KEY idx_created (created_at), KEY (customer_id));
CREATE TABLE items (id INT PRIMARY KEY AUTO_INCREMENT, order_id INT, sku VARCHAR(20), qty INT, KEY (order_id));
INSERT INTO customers (email, country) WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM s WHERE n < 20000) SELECT CONCAT('u', n, '@x.io'), ELT(1 + n % 5, 'SK','CZ','DE','AT','PL') FROM s;
INSERT INTO orders (customer_id, status, total, created_at) WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM s WHERE n < 60000) SELECT 1 + n % 20000, ELT(1 + n % 3, 'paid','shipped','new'), n % 500, '2026-01-01' + INTERVAL n MINUTE FROM s;
INSERT INTO items (order_id, sku, qty) WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM s WHERE n < 120000) SELECT 1 + n % 60000, CONCAT('SKU', n % 300), 1 + n % 4 FROM s;
ANALYZE TABLE customers, orders, items;
```
(MySQL needs `SET SESSION cte_max_recursion_depth = 200000`, MariaDB `SET SESSION max_recursive_iterations = 200000`.)

## Queries

| Name | Statement |
|---|---|
| by_key| `SELECT * FROM customers WHERE id = 42` |
| full_scan| `SELECT * FROM customers WHERE email LIKE '%9@x.io'` |
| join2| `SELECT o.id, c.email, o.status, o.total FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.created_at >= '2026-02-10' AND o.status IN ('paid', 'shipped') ORDER BY o.created_at DESC LIMIT 500` |
| join3| `SELECT c.country, COUNT(*) AS n, SUM(i.qty) AS qty FROM customers c JOIN orders o ON o.customer_id = c.id JOIN items i ON i.order_id = o.id WHERE o.status = 'paid' GROUP BY c.country` |
| subquery| `SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS n FROM customers c WHERE c.country = 'SK'` |
| semijoin| `SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders WHERE total > 495)` |
| derived| `SELECT t.customer_id, t.n FROM (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) t WHERE t.n > 3` |
| union| `SELECT email FROM customers WHERE country = 'SK' UNION SELECT status FROM orders WHERE id < 100` |
| group_filesort| `SELECT sku, SUM(qty) AS s FROM items GROUP BY sku ORDER BY s DESC` |
| window| `SELECT id, total, RANK() OVER (PARTITION BY status ORDER BY total DESC) AS r FROM orders WHERE id < 1000` |
| update| `UPDATE orders SET total = total + 1 WHERE created_at < '2026-01-02'` |
| update_join| `UPDATE orders o JOIN customers c ON c.id = o.customer_id SET o.total = o.total + 1 WHERE c.country = 'SK' AND o.created_at < '2026-01-05'` |

## Oracle

Oracle Database Free 23 (`gvenzl/oracle-free:23-slim`), schema `P11_PLANS` with the same three tables and data (`CONNECT BY LEVEL` instead of recursive CTEs, `DATE` instead of `DATETIME`, statistics from `DBMS_STATS.GATHER_SCHEMA_STATS`). Same queries, in Oracle syntax: `join2` ends `FETCH FIRST 500 ROWS ONLY`, `update_join` is `UPDATE orders o SET o.total = o.total + 1 WHERE o.created_at < DATE '2026-01-05' AND o.customer_id IN (SELECT id FROM customers WHERE country = 'SK')`; one more, `with`: `WITH big AS (SELECT customer_id FROM orders WHERE total > 490) SELECT c.email FROM customers c JOIN big b ON b.customer_id = c.id`.

- `oracle_<query>.json`: `EXPLAIN PLAN` rows read with `ESTIMATED_ROWS` (`src/oracle/plan.rs`), one JSON object per step, joined into an array
- `oracle_<query>.txt`: `DBMS_XPLAN.DISPLAY('PLAN_TABLE', …, 'TYPICAL')`
- `oracle_<query>.analyze.json`: the run's `V$SQL_PLAN_STATISTICS_ALL` rows read with `MEASURED_ROWS` (joined the same way), after running the statement with `/*+ GATHER_PLAN_STATISTICS */` (`with`: `STATISTICS_LEVEL = ALL`)
- `oracle_<query>.analyze.txt`: `DBMS_XPLAN.DISPLAY_CURSOR(…, 'ALLSTATS LAST')`

Measured updates were rolled back.
