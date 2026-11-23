//! The plan parsers against output captured from MySQL 8.4 and MariaDB 11.8 in Docker
//! (`tests/fixtures/plans/`, schema in the fixtures' README).

use std::path::Path;

use deadlock_core::plan::{self, Op, Plan, PlanNode};

const QUERIES: [&str; 12] = [
    "by_key",
    "full_scan",
    "join2",
    "join3",
    "subquery",
    "semijoin",
    "derived",
    "union",
    "group_filesort",
    "window",
    "update",
    "update_join",
];

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plans")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn find(p: &Plan, f: impl Fn(&PlanNode) -> bool) -> Option<&PlanNode> {
    p.walk().into_iter().map(|(_, n)| n).find(|n| f(n))
}

fn table<'a>(p: &'a Plan, name: &str) -> &'a PlanNode {
    find(p, |n| n.table.as_deref() == Some(name)).unwrap_or_else(|| panic!("no step on {name}"))
}

#[test]
fn every_fixture_parses() {
    for q in QUERIES {
        for flavor in ["mysql", "mariadb"] {
            let p = plan::json(&fixture(&format!("{flavor}_{q}.json")), false).unwrap();
            assert!(!p.steps.is_empty(), "{flavor} {q}");
            assert!(!p.measured && p.execution_ms.is_none() && p.slowest().is_none());
            // Estimated plans carry no measured values anywhere.
            assert!(
                p.walk()
                    .iter()
                    .all(|(_, n)| n.actual_rows.is_none() && n.own_ms.is_none())
            );
        }
        let p = plan::json(&fixture(&format!("mariadb_{q}.analyze.json")), true).unwrap();
        assert!(
            p.measured && p.execution_ms.is_some_and(|t| t > 0.),
            "mariadb {q}"
        );
        assert!(p.planning_ms.is_some(), "mariadb {q}");
        let tree = fixture(&format!("mysql_{q}.analyze.txt"));
        if q == "update" {
            // MySQL does not measure single-table UPDATE.
            assert!(plan::mysql_tree(&tree).is_err());
            continue;
        }
        let p = plan::mysql_tree(&tree).unwrap();
        assert!(p.measured && p.execution_ms.is_some(), "mysql {q}");
        assert_eq!(p.rows_returned.is_some(), q != "update_join", "mysql {q}");
        assert_eq!(p.raw, tree);
    }
}

#[test]
fn mysql_estimated_join_has_its_tables_and_costs() {
    let p = plan::json(&fixture("mysql_join3.json"), false).unwrap();
    assert_eq!(p.cost, Some(12086.17));
    let block = &p.steps[0];
    assert_eq!((block.op, block.title.as_str()), (Op::Query, "Select #1"));
    let group = &block.children[0];
    assert_eq!(
        (group.op, group.detail.as_str()),
        (Op::Group, "using a temporary table")
    );
    let join = &group.children[0];
    assert_eq!(join.op, Op::Join);
    let names: Vec<_> = join
        .children
        .iter()
        .map(|c| c.table.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(names, ["o", "c", "i"]);
    let o = &join.children[0];
    assert_eq!(
        (o.op, o.title.as_str(), o.estimated_rows),
        (Op::FullScan, "Full table scan", Some(60021.))
    );
    assert!(
        o.facts
            .iter()
            .any(|(l, v)| l == "Rows kept (estimate)" && v == "10%")
    );
    assert!(o.facts.iter().any(|(l, _)| l == "Condition"));
    let c = &join.children[1];
    assert_eq!(
        (c.op, c.index.as_deref(), c.detail.as_str()),
        (Op::Lookup, Some("PRIMARY"), "on c using PRIMARY")
    );
    assert!((c.cost.unwrap() - 2100.74).abs() < 1e-6);
}

#[test]
fn mysql_estimated_shapes() {
    let p = plan::json(&fixture("mysql_union.json"), false).unwrap();
    let union = find(&p, |n| n.op == Op::Union).unwrap();
    assert_eq!(union.children.len(), 2);
    assert!(union.children.iter().all(|c| c.op == Op::Query));
    let p = plan::json(&fixture("mysql_derived.json"), false).unwrap();
    let t = table(&p, "t");
    assert_eq!(t.children[0].op, Op::Materialize);
    assert!(find(&p, |n| n.table.as_deref() == Some("orders")).is_some());
    let p = plan::json(&fixture("mysql_subquery.json"), false).unwrap();
    let block = &p.steps[0];
    assert_eq!(block.children.len(), 2);
    assert_eq!(block.children[1].title, "Select #2");
    assert!(
        block.children[1]
            .facts
            .iter()
            .any(|(l, v)| l == "Dependent" && v == "yes")
    );
    let p = plan::json(&fixture("mysql_window.json"), false).unwrap();
    let w = find(&p, |n| n.op == Op::Window).unwrap();
    assert!(w.facts.iter().any(|(l, v)| l == "Functions" && v == "rank"));
    assert_eq!(w.children[0].table.as_deref(), Some("orders"));
    let p = plan::json(&fixture("mysql_group_filesort.json"), false).unwrap();
    let sort = find(&p, |n| n.op == Op::Sort).unwrap();
    assert_eq!(sort.title, "Sort");
    assert_eq!(sort.children[0].op, Op::Group);
    let p = plan::json(&fixture("mysql_update.json"), false).unwrap();
    let u = table(&p, "orders");
    assert_eq!((u.op, u.title.as_str()), (Op::Write, "Update"));
    assert_eq!(u.detail, "orders via index range scan using idx_created");
}

#[test]
fn mysql_measured_tree_derives_own_time() {
    let p = plan::mysql_tree(&fixture("mysql_join3.analyze.txt")).unwrap();
    assert_eq!(p.rows_returned, Some(5.));
    assert_eq!(p.execution_ms, Some(884.));
    let scan = table(&p, "o");
    assert_eq!(
        (scan.op, scan.title.as_str(), scan.detail.as_str()),
        (Op::FullScan, "Table scan", "on o")
    );
    assert_eq!(
        (scan.estimated_rows, scan.actual_rows),
        (Some(60021.), Some(60000.))
    );
    assert_eq!(scan.own_ms, Some(94.3));
    // Filter: 214 inclusive, its scan 94.3.
    let filter = find(&p, |n| n.op == Op::Filter).unwrap();
    assert!((filter.own_ms.unwrap() - 119.7).abs() < 1e-9);
    // The lookup on i runs 20 000 times at 0.00766 ms to its last row.
    let i = table(&p, "i");
    assert!((i.own_ms.unwrap() - 153.2).abs() < 1e-9);
    assert_eq!(i.loops, Some(20000.));
    let slowest = p.walk()[p.slowest().unwrap()].1;
    assert_eq!(slowest.title, "Nested loop inner join");
    let share: f64 = p.walk().iter().filter_map(|(_, n)| n.own_ms).sum();
    assert!((share - p.own_total()).abs() < 1e-9);

    let p = plan::mysql_tree(&fixture("mysql_subquery.analyze.txt")).unwrap();
    assert_eq!(p.steps.len(), 2);
    assert_eq!(
        (p.steps[1].op, p.steps[1].title.as_str()),
        (Op::Query, "Select #2 (subquery in projection; dependent)")
    );
    let p = plan::mysql_tree(&fixture("mysql_update_join.analyze.txt")).unwrap();
    assert_eq!(
        (p.steps[0].op, p.steps[0].table.as_deref()),
        (Op::Write, Some("o"))
    );
    let p = plan::mysql_tree(&fixture("mysql_semijoin.analyze.txt")).unwrap();
    let lookup = table(&p, "<subquery2>");
    assert!(lookup.misestimated() || lookup.actual_rows == Some(0.008));
    assert_eq!(lookup.children[0].op, Op::Materialize);
}

#[test]
fn mariadb_measured_reports_engine_time() {
    let p = plan::json(&fixture("mariadb_join3.analyze.json"), true).unwrap();
    assert!((p.execution_ms.unwrap() - 78.743_442_75).abs() < 1e-9);
    assert!((p.planning_ms.unwrap() - 0.140_626_242).abs() < 1e-9);
    assert_eq!(p.rows_returned, None);
    let o = table(&p, "o");
    assert_eq!(
        (o.estimated_rows, o.actual_rows, o.loops),
        (Some(2.), Some(3.), Some(20000.))
    );
    assert!((o.own_ms.unwrap() - (34.224_472_83 + 3.134_883_304)).abs() < 1e-9);
    assert!(
        o.facts
            .iter()
            .any(|(l, v)| l == "Pages accessed" && v == "160 124")
    );
    assert!(
        o.facts
            .iter()
            .any(|(l, v)| l == "Rows kept" && v == "33.33%")
    );
    // The block's own time is what its tables do not account for.
    let block = &p.steps[0];
    let tables: f64 = p.walk().iter().skip(1).filter_map(|(_, n)| n.own_ms).sum();
    assert!((block.own_ms.unwrap() - (78.743_442_75 - tables)).abs() < 1e-9);

    let p = plan::json(&fixture("mariadb_group_filesort.analyze.json"), true).unwrap();
    let sort = find(&p, |n| n.op == Op::Sort).unwrap();
    assert_eq!(
        (sort.detail.as_str(), sort.actual_rows),
        ("sum(items.qty) desc", Some(300.))
    );
    assert_eq!(sort.children[0].title, "Temporary table");
    let p = plan::json(&fixture("mariadb_union.analyze.json"), true).unwrap();
    let union = find(&p, |n| n.op == Op::Union).unwrap();
    assert_eq!((union.actual_rows, union.children.len()), (Some(4003.), 2));
    let p = plan::json(&fixture("mariadb_derived.analyze.json"), true).unwrap();
    let d = table(&p, "<derived2>");
    assert_eq!(d.children[0].op, Op::Materialize);
    assert!(d.misestimated());
    let p = plan::json(&fixture("mariadb_subquery.analyze.json"), true).unwrap();
    assert!(find(&p, |n| n.title == "Subquery cache").is_some());
    let p = plan::json(&fixture("mariadb_window.analyze.json"), true).unwrap();
    assert!(find(&p, |n| n.op == Op::Window).is_some());
    let p = plan::json(&fixture("mariadb_update.analyze.json"), true).unwrap();
    let u = table(&p, "orders");
    assert_eq!((u.op, u.actual_rows), (Op::Write, Some(1439.)));
    assert!(u.facts.iter().any(|(l, _)| l == "Pages updated"));
}

#[test]
fn descriptions_name_tables_and_indexes() {
    let p = plan::mysql_tree(&fixture("mysql_join3.analyze.txt")).unwrap();
    assert_eq!(
        table(&p, "o").describe(Some("orders")),
        "Reads every row of orders."
    );
    assert_eq!(
        table(&p, "c").describe(Some("customers")),
        "Looks up at most one row of customers through index PRIMARY. It ran 20 000 times."
    );
    let p = plan::json(&fixture("mysql_group_filesort.json"), false).unwrap();
    assert!(
        find(&p, |n| n.op == Op::Sort)
            .unwrap()
            .describe(None)
            .starts_with("Sorts the rows")
    );
}

/// Oracle 23 Free: `oracle_<query>.json` (PLAN_TABLE rows), `.txt` (DBMS_XPLAN.DISPLAY),
/// `.analyze.json` (V$SQL_PLAN_STATISTICS_ALL rows) and `.analyze.txt`
/// (DISPLAY_CURSOR … ALLSTATS LAST).
#[test]
fn oracle_fixtures_parse() {
    let mut queries = QUERIES.to_vec();
    queries.push("with");
    for q in queries {
        let raw = fixture(&format!("oracle_{q}.txt"));
        let p = plan::oracle(&fixture(&format!("oracle_{q}.json")), false, raw.clone()).unwrap();
        assert_eq!(p.steps.len(), 1, "{q}");
        assert_eq!(p.steps[0].op, Op::Query, "{q}");
        assert!(
            !p.measured && p.execution_ms.is_none() && p.cost.is_some(),
            "{q}"
        );
        assert!(
            p.walk()
                .iter()
                .all(|(_, n)| n.actual_rows.is_none() && n.own_ms.is_none())
        );
        assert_eq!(p.raw, raw);
        // Every step DBMS_XPLAN shows is in the tree, in the same order.
        let shown = raw
            .lines()
            .filter(|l| l.starts_with('|') && l.contains(" | ") && !l.contains("Operation"))
            .count();
        assert_eq!(p.walk().len(), shown, "{q}");
        let m = plan::oracle(
            &fixture(&format!("oracle_{q}.analyze.json")),
            true,
            String::new(),
        )
        .unwrap();
        assert!(m.measured && m.execution_ms.is_some_and(|t| t > 0.), "{q}");
        assert!(m.rows_returned.is_some(), "{q}");
        assert!(
            m.walk()
                .iter()
                .skip(1)
                .all(|(_, n)| n.loops.is_some() && n.actual_rows.is_some()),
            "{q}"
        );
        assert!(m.slowest().is_some(), "{q}");
    }
}

#[test]
fn oracle_steps_name_their_tables_and_ops() {
    let p = plan::oracle(&fixture("oracle_by_key.json"), false, String::new()).unwrap();
    let lookup = find(&p, |n| n.title == "INDEX UNIQUE SCAN").unwrap();
    assert_eq!(lookup.op, Op::Lookup);
    assert!(
        lookup
            .index
            .as_deref()
            .is_some_and(|i| i.starts_with("SYS_C"))
    );
    assert_eq!(lookup.table.as_deref(), Some("CUSTOMERS"));
    assert!(
        lookup
            .facts
            .iter()
            .any(|(l, v)| l == "Access" && v.contains("\"ID\"=42"))
    );
    assert_eq!(table(&p, "CUSTOMERS").op, Op::Lookup);

    let p = plan::oracle(&fixture("oracle_join3.analyze.json"), true, String::new()).unwrap();
    assert_eq!(table(&p, "I").title, "TABLE ACCESS FULL");
    assert_eq!(table(&p, "I").op, Op::FullScan);
    assert_eq!(table(&p, "I").actual_rows, Some(120_000.));
    let group = find(&p, |n| n.title == "HASH GROUP BY").unwrap();
    assert_eq!(group.op, Op::Group);
    assert_eq!(p.rows_returned, Some(5.));
    assert!(find(&p, |n| n.title == "HASH JOIN").is_some_and(|n| n.op == Op::Join));

    let p = plan::oracle(&fixture("oracle_update.analyze.json"), true, String::new()).unwrap();
    assert_eq!(p.steps[0].children[0].op, Op::Write);
    let p = plan::oracle(&fixture("oracle_join2.json"), false, String::new()).unwrap();
    assert!(find(&p, |n| n.op == Op::Sort || n.op == Op::Limit).is_some());
    assert!(find(&p, |n| n.op == Op::RangeScan).is_some());
    let p = plan::oracle(&fixture("oracle_window.json"), false, String::new()).unwrap();
    assert!(find(&p, |n| n.op == Op::Window).is_some());
    let p = plan::oracle(&fixture("oracle_union.json"), false, String::new()).unwrap();
    assert!(find(&p, |n| n.op == Op::Union || n.op == Op::Sort).is_some());

    // Unknown operations are generic steps; bad input is an error, not a panic.
    let p = plan::oracle(r#"[{"id":0,"operation":"SELECT STATEMENT"},{"id":1,"parent_id":0,"operation":"XPLAN NEW THING","options":"FAST"}]"#, false, String::new()).unwrap();
    assert_eq!(p.steps[0].children[0].op, Op::Other);
    assert!(
        p.steps[0].children[0]
            .describe(None)
            .contains("XPLAN NEW THING FAST")
    );
    assert!(plan::oracle("not json", false, String::new()).is_err());
    assert!(plan::oracle("[]", false, String::new()).is_err());
}
