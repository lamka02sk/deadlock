//! Unit tests for plans: the "How each change is applied" table, per engine.

use super::*;

fn def(
    flavor: Flavor,
    schema: &str,
    name: &str,
    kind: ObjectKind,
    props: Properties,
) -> ObjectDefinition {
    ObjectDefinition {
        object: ObjectRef::new(schema, name, kind),
        flavor,
        props,
        source: String::new(),
        restore: vec![format!("CREATE {} original", kind.keyword())],
        read_only: None,
        status: Vec::new(),
        errors: Vec::new(),
        grants: Vec::new(),
        facts: Vec::new(),
        siblings: Vec::new(),
        notices: Vec::new(),
        own_schema: true,
    }
}

fn with(d: &ObjectDefinition, f: impl FnOnce(&mut Properties)) -> ObjectDraft {
    let mut draft = d.draft();
    f(&mut draft.props);
    draft
}

fn audit_trigger(flavor: Flavor) -> ObjectDefinition {
    def(
        flavor,
        "warehouse",
        "orders_audit_bu",
        ObjectKind::Trigger,
        Properties::Trigger(TriggerProps {
            table: "orders".into(),
            timing: Timing::Before,
            events: vec![TriggerEvent::Update],
            for_each_row: true,
            order: Some((TriggerOrder::Follows, "orders_touch_bu".into())),
            definer: "shop_admin@%".into(),
            enabled: true,
            body: "BEGIN\n  IF NEW.status <> OLD.status THEN\n    SET @x = 1;\n  END IF;\nEND"
                .into(),
            ..TriggerProps::default()
        }),
    )
}

fn new_body(p: &mut Properties) {
    if let Properties::Trigger(t) = p {
        t.body = "BEGIN\n  IF NEW.status <> OLD.status\n    AND NEW.status <> 'draft'\n  THEN\n    SET @x = 1;\n  END IF;\nEND".into();
    }
}

#[test]
fn mysql_trigger_change_checks_on_an_empty_copy_then_swaps() {
    let d = audit_trigger(Flavor::Mysql);
    let p = plan(Flavor::Mysql, Some(&d), &with(&d, new_body)).unwrap();
    let kinds: Vec<_> = p.steps.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [StepKind::Check, StepKind::Swap, StepKind::Restore]);
    assert!(
        !p.atomic
            && p.non_atomic_warning()
                .unwrap()
                .starts_with("Not atomic: MySQL")
    );
    assert_eq!((p.statement_count(), p.restore_count()), (5, 1));
    assert_eq!(p.changes, ["Body"]);
    assert_eq!(p.verb(), "DROP + CREATE");
    let check = &p.steps[0];
    assert!(check.skippable);
    assert_eq!(
        check.statements[0],
        "CREATE TABLE `warehouse`.`_dl_check_orders` LIKE `warehouse`.`orders`"
    );
    assert!(check.statements[1].starts_with(
        "CREATE DEFINER=`shop_admin`@`%` TRIGGER `warehouse`.`_dl_check_orders_audit_bu`\n\
         BEFORE UPDATE ON `warehouse`.`_dl_check_orders` FOR EACH ROW\nBEGIN"
    ));
    assert_eq!(check.cleanup, ["DROP TABLE `warehouse`.`_dl_check_orders`"]);
    let swap = &p.steps[1];
    assert_eq!(
        swap.statements[0],
        "DROP TRIGGER `warehouse`.`orders_audit_bu`"
    );
    assert!(swap.statements[1].contains(
        "BEFORE UPDATE ON `warehouse`.`orders` FOR EACH ROW FOLLOWS `orders_touch_bu`\nBEGIN"
    ));
    assert_eq!(p.steps[2].statements, ["CREATE TRIGGER original"]);
    let preview = p.preview();
    assert!(preview.starts_with("-- Not atomic: MySQL commits DDL immediately"));
    assert!(preview.contains("-- 1. Check the new definition on an empty copy of orders"));
    assert!(preview.contains("-- 3. Only if step 2's CREATE fails: restore the original"));
    assert!(preview.contains("\n-- CREATE TRIGGER original;\n"));
    assert!(preview.contains("\nDROP TRIGGER `warehouse`.`orders_audit_bu`;\n"));
}

#[test]
fn mariadb_replaces_triggers_routines_and_views_in_one_statement() {
    let d = audit_trigger(Flavor::Mariadb);
    let p = plan(Flavor::Mariadb, Some(&d), &with(&d, new_body)).unwrap();
    assert_eq!(p.steps.len(), 1);
    assert!(p.atomic && p.non_atomic_warning().is_none());
    assert!(p.steps[0].statements[0].starts_with(
        "CREATE OR REPLACE DEFINER=`shop_admin`@`%` TRIGGER `warehouse`.`orders_audit_bu`\n"
    ));
    assert!(
        p.preview()
            .starts_with("-- MariaDB replaces the trigger in one statement")
    );

    let r = restock(Flavor::Mariadb);
    let p = plan(
        Flavor::Mariadb,
        Some(&r),
        &with(&r, |p| {
            if let Properties::Routine(r) = p {
                r.body = "BEGIN\n  SELECT 2;\nEND".into();
            }
        }),
    )
    .unwrap();
    assert_eq!(p.verb(), "CREATE OR REPLACE");
    assert_eq!(
        p.steps[0].statements[0],
        "CREATE OR REPLACE DEFINER=`billing_app`@`%` PROCEDURE `billing`.`restock_product`(\n  \
         IN `p_product_id` BIGINT UNSIGNED,\n  OUT `p_new_stock` INT\n)\nMODIFIES SQL DATA\n\
         SQL SECURITY DEFINER\nCOMMENT 'Adds stock'\nBEGIN\n  SELECT 2;\nEND"
    );
}

fn restock(flavor: Flavor) -> ObjectDefinition {
    let mut d = def(
        flavor,
        "billing",
        "restock_product",
        ObjectKind::Procedure,
        Properties::Routine(RoutineProps {
            params: vec![
                Param {
                    mode: ParamMode::In,
                    name: "p_product_id".into(),
                    data_type: "BIGINT UNSIGNED".into(),
                    ..Param::default()
                },
                Param {
                    mode: ParamMode::Out,
                    name: "p_new_stock".into(),
                    data_type: "INT".into(),
                    ..Param::default()
                },
            ],
            data_access: Some(DataAccess::ModifiesSqlData),
            security: Security::Definer,
            comment: "Adds stock".into(),
            definer: "billing_app@%".into(),
            body: "BEGIN\n  SELECT 1;\nEND".into(),
            ..RoutineProps::default()
        }),
    );
    d.grants = vec![Grant {
        user: "app".into(),
        host: "%".into(),
        privileges: vec!["EXECUTE".into()],
        grant_option: false,
    }];
    d
}

#[test]
fn mysql_routine_change_checks_under_a_temporary_name_and_grants_again() {
    let r = restock(Flavor::Mysql);
    let p = plan(
        Flavor::Mysql,
        Some(&r),
        &with(&r, |p| {
            if let Properties::Routine(r) = p {
                r.params.pop();
            }
        }),
    )
    .unwrap();
    let kinds: Vec<_> = p.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [
            StepKind::Check,
            StepKind::Swap,
            StepKind::Grants,
            StepKind::Restore,
            StepKind::RestoreGrants
        ]
    );
    assert!(p.steps[0].statements[0].contains("PROCEDURE `billing`.`_dl_check_restock_product`("));
    assert_eq!(
        p.steps[0].cleanup,
        ["DROP PROCEDURE `billing`.`_dl_check_restock_product`"]
    );
    assert_eq!(
        p.steps[2].statements,
        ["GRANT EXECUTE ON PROCEDURE `billing`.`restock_product` TO 'app'@'%'"]
    );
    assert!(p.preview().contains("GRANT EXECUTE ON PROCEDURE"));
    assert_eq!(p.changes, ["Parameters"]);
}

#[test]
fn characteristics_only_is_one_alter() {
    for flavor in [Flavor::Mysql, Flavor::Mariadb] {
        let r = restock(flavor);
        let p = plan(
            flavor,
            Some(&r),
            &with(&r, |p| {
                if let Properties::Routine(r) = p {
                    r.data_access = Some(DataAccess::ReadsSqlData);
                    r.comment = "Order total incl. lines and shipping".into();
                }
            }),
        )
        .unwrap();
        assert_eq!(p.steps.len(), 1);
        assert_eq!(
            p.steps[0].statements[0],
            "ALTER PROCEDURE `billing`.`restock_product`\nREADS SQL DATA\n\
             COMMENT 'Order total incl. lines and shipping'"
        );
        assert!(p.atomic);
    }
    let r = restock(Flavor::Mysql);
    let p = plan(Flavor::Mysql, Some(&r), &r.draft()).unwrap();
    assert!(p.is_empty() && p.preview() == "-- No changes");
}

#[test]
fn events_alter_every_field() {
    let d = def(
        Flavor::Mysql,
        "warehouse",
        "nightly_rollup",
        ObjectKind::Event,
        Properties::Event(EventProps {
            schedule: Schedule::Every {
                value: "1".into(),
                unit: IntervalUnit::Day,
                starts: "2026-11-24 02:00:00".into(),
                ends: String::new(),
            },
            body: "CALL rebuild_stats(CURRENT_DATE - INTERVAL 1 DAY)".into(),
            ..EventProps::default()
        }),
    );
    let mut draft = with(&d, |p| {
        if let Properties::Event(e) = p {
            e.schedule = Schedule::Every {
                value: "1".into(),
                unit: IntervalUnit::Day,
                starts: "2026-11-25 02:00:00".into(),
                ends: String::new(),
            };
        }
    });
    let p = plan(Flavor::Mysql, Some(&d), &draft).unwrap();
    assert_eq!(
        p.steps[0].statements,
        [
            "ALTER EVENT `warehouse`.`nightly_rollup`\nON SCHEDULE EVERY 1 DAY STARTS '2026-11-25 02:00:00'"
        ]
    );
    draft.object.name = "rollup".into();
    if let Properties::Event(e) = &mut draft.props {
        e.status = EventStatus::DisableOnReplica;
        e.preserve = true;
    }
    let p = plan(Flavor::Mariadb, Some(&d), &draft).unwrap();
    let s = &p.steps[0].statements[0];
    assert!(
        s.contains("\nON COMPLETION PRESERVE\nRENAME TO `warehouse`.`rollup`\nDISABLE ON SLAVE")
    );
    let p = plan(Flavor::Mysql, Some(&d), &draft).unwrap();
    assert!(p.steps[0].statements[0].ends_with("DISABLE ON REPLICA"));
    let created = plan(Flavor::Mysql, None, &draft).unwrap();
    assert_eq!(
        created.steps[0].statements[0],
        "CREATE EVENT `warehouse`.`rollup`\nON SCHEDULE EVERY 1 DAY STARTS '2026-11-25 02:00:00'\n\
         ON COMPLETION PRESERVE\nDISABLE ON REPLICA\nDO CALL rebuild_stats(CURRENT_DATE - INTERVAL 1 DAY)"
    );
}

#[test]
fn sequences_alter_what_changed() {
    let d = def(
        Flavor::Mariadb,
        "billing",
        "invoice_no_seq",
        ObjectKind::Sequence,
        Properties::Sequence(SequenceProps {
            start: "1".into(),
            increment: "2".into(),
            cache: "1000".into(),
            ..SequenceProps::default()
        }),
    );
    let draft = with(&d, |p| {
        if let Properties::Sequence(s) = p {
            s.increment = "1".into();
            s.cache = "500".into();
            s.restart_with = "10".into();
        }
    });
    let p = plan(Flavor::Mariadb, Some(&d), &draft).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["ALTER SEQUENCE `billing`.`invoice_no_seq` INCREMENT BY 1 CACHE 500 RESTART WITH 10"]
    );
    assert!(plan(Flavor::Mysql, Some(&d), &draft).is_err());
    let o = def(
        Flavor::Oracle,
        "BILLING",
        "INVOICE_SEQ",
        ObjectKind::Sequence,
        d.props.clone(),
    );
    let p = plan(
        Flavor::Oracle,
        Some(&o),
        &with(&o, |p| {
            if let Properties::Sequence(s) = p {
                s.cache = "100".into();
                s.max = String::new();
            }
        }),
    )
    .unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["ALTER SEQUENCE BILLING.INVOICE_SEQ CACHE 100"]
    );
    let p = plan(Flavor::Oracle, Some(&o), &draft).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["RENAME INVOICE_SEQ TO INVOICE_NO_SEQ"]
    );
    assert!(p.steps[1].statements[0].ends_with("RESTART START WITH 10"));
    let start = with(&o, |p| {
        if let Properties::Sequence(s) = p {
            s.start = "5".into();
        }
    });
    assert!(
        plan(Flavor::Oracle, Some(&o), &start)
            .unwrap_err()
            .contains("Restart with")
    );
    let bad = with(&o, |p| {
        if let Properties::Sequence(s) = p {
            s.cache = "1; DROP TABLE x".into();
        }
    });
    assert!(plan(Flavor::Oracle, Some(&o), &bad).is_err());
}

#[test]
fn views_replace_and_rename() {
    let d = def(
        Flavor::Mysql,
        "warehouse",
        "open_orders",
        ObjectKind::View,
        Properties::View(ViewProps {
            algorithm: ViewAlgorithm::Merge,
            definer: "shop_admin@%".into(),
            security: Security::Definer,
            check: ViewCheck::None,
            query: "SELECT id FROM orders".into(),
            ..ViewProps::default()
        }),
    );
    let mut draft = with(&d, |p| {
        if let Properties::View(v) = p {
            v.check = ViewCheck::Cascaded;
            v.query = "SELECT id\nFROM orders\nWHERE status IN ('pending', 'paid');".into();
        }
    });
    let p = plan(Flavor::Mysql, Some(&d), &draft).unwrap();
    assert_eq!(
        p.steps[0].statements[0],
        "CREATE OR REPLACE ALGORITHM=MERGE DEFINER=`shop_admin`@`%` SQL SECURITY DEFINER VIEW \
         `warehouse`.`open_orders` AS\nSELECT id\nFROM orders\nWHERE status IN ('pending', 'paid')\n\
         WITH CASCADED CHECK OPTION"
    );
    draft.object.name = "orders_open".into();
    let p = plan(Flavor::Mysql, Some(&d), &draft).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["RENAME TABLE `warehouse`.`open_orders` TO `warehouse`.`orders_open`"]
    );
    assert!(p.steps[1].statements[0].contains("VIEW `warehouse`.`orders_open` AS"));
    let r = rename_plan(&d, "v2").unwrap();
    assert_eq!(r.steps[0].kind, StepKind::Rename);
    assert_eq!(r.previous.as_ref().unwrap().name, "open_orders");
    let o = def(
        Flavor::Oracle,
        "BILLING",
        "OPEN_V",
        ObjectKind::View,
        Properties::View(ViewProps {
            check: ViewCheck::ReadOnly,
            query: "SELECT 1 x FROM dual".into(),
            ..ViewProps::default()
        }),
    );
    let p = plan(Flavor::Oracle, None, &o.draft()).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["CREATE VIEW BILLING.OPEN_V AS\nSELECT 1 x FROM dual\nWITH READ ONLY"]
    );
    assert_eq!(
        rename_plan(&o, "new_v").unwrap().steps[0].statements,
        ["RENAME OPEN_V TO NEW_V"]
    );
    let mut theirs = o.clone();
    theirs.own_schema = false;
    assert!(rename_plan(&theirs, "x").is_err());
}

fn oracle_trigger() -> ObjectDefinition {
    def(
        Flavor::Oracle,
        "BILLING",
        "AUDIT_INVOICES_BIU",
        ObjectKind::Trigger,
        Properties::Trigger(TriggerProps {
            table: "INVOICES".into(),
            timing: Timing::Before,
            events: vec![TriggerEvent::Insert, TriggerEvent::Update],
            update_columns: vec!["STATUS".into()],
            for_each_row: true,
            when: "new.status <> 'draft'".into(),
            order: Some((TriggerOrder::Follows, "INVOICES_STAMP_BIU".into())),
            enabled: true,
            body: "BEGIN\n  NULL;\nEND;".into(),
            ..TriggerProps::default()
        }),
    )
}

#[test]
fn oracle_triggers_alter_enable_rename_and_replace() {
    let d = oracle_trigger();
    let off = with(&d, |p| {
        if let Properties::Trigger(t) = p {
            t.enabled = false;
        }
    });
    let p = plan(Flavor::Oracle, Some(&d), &off).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["ALTER TRIGGER BILLING.AUDIT_INVOICES_BIU DISABLE"]
    );
    let mut renamed = d.draft();
    renamed.object.name = "audit_biu".into();
    let p = plan(Flavor::Oracle, Some(&d), &renamed).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["ALTER TRIGGER BILLING.AUDIT_INVOICES_BIU RENAME TO AUDIT_BIU"]
    );
    assert_eq!(p.object.name, "AUDIT_BIU");
    let body = with(&d, |p| {
        if let Properties::Trigger(t) = p {
            t.body = "BEGIN\n  :NEW.x := 1;\nEND;".into();
            t.enabled = false;
        }
    });
    let p = plan(Flavor::Oracle, Some(&d), &body).unwrap();
    assert_eq!(
        p.steps[0].statements[0],
        "CREATE OR REPLACE TRIGGER BILLING.AUDIT_INVOICES_BIU\nBEFORE INSERT OR UPDATE OF STATUS \
         ON BILLING.INVOICES\nFOR EACH ROW\nFOLLOWS BILLING.INVOICES_STAMP_BIU\nDISABLE\n\
         WHEN (new.status <> 'draft')\nBEGIN\n  :NEW.x := 1;\nEND;"
    );
    assert!(p.preview().ends_with("END;\n/\n"));
}

#[test]
fn oracle_functions_and_packages_replace_their_source() {
    let f = def(
        Flavor::Oracle,
        "BILLING",
        "CALC_LATE_FEE",
        ObjectKind::Function,
        Properties::Routine(RoutineProps {
            params: vec![
                Param {
                    name: "P_INVOICE_ID".into(),
                    data_type: "NUMBER".into(),
                    ..Param::default()
                },
                Param {
                    name: "p_as_of".into(),
                    data_type: "DATE".into(),
                    default: "SYSDATE".into(),
                    ..Param::default()
                },
                Param {
                    mode: ParamMode::Out,
                    name: "P_DAYS_LATE".into(),
                    data_type: "PLS_INTEGER".into(),
                    ..Param::default()
                },
            ],
            returns: "NUMBER".into(),
            deterministic: true,
            body: "BEGIN\n  RETURN 0;\nEND;".into(),
            ..RoutineProps::default()
        }),
    );
    let p = plan(Flavor::Oracle, None, &f.draft()).unwrap();
    assert_eq!(
        p.steps[0].statements[0],
        "CREATE FUNCTION BILLING.CALC_LATE_FEE(\n  P_INVOICE_ID IN NUMBER,\n  P_AS_OF IN DATE \
         DEFAULT SYSDATE,\n  P_DAYS_LATE OUT PLS_INTEGER\n) RETURN NUMBER AUTHID DEFINER \
         DETERMINISTIC\nIS\nBEGIN\n  RETURN 0;\nEND;"
            .replace(") RETURN", ")\nRETURN")
    );
    let changed = with(&f, |p| {
        if let Properties::Routine(r) = p {
            r.result_cache = true;
        }
    });
    let p = plan(Flavor::Oracle, Some(&f), &changed).unwrap();
    assert!(p.steps[0].statements[0].starts_with("CREATE OR REPLACE FUNCTION"));
    assert!(p.atomic);

    let pkg = def(
        Flavor::Oracle,
        "BILLING",
        "INVOICE_PKG",
        ObjectKind::Package,
        Properties::Source(SourceProps {
            spec: "PACKAGE invoice_pkg AS\n  PROCEDURE issue;\nEND;".into(),
            body: "PACKAGE BODY invoice_pkg AS\n  PROCEDURE issue IS BEGIN NULL; END;\nEND;".into(),
        }),
    );
    let spec = with(&pkg, |p| {
        if let Properties::Source(s) = p {
            s.spec = "PACKAGE invoice_pkg AS\n  PROCEDURE issue;\n  PROCEDURE void;\nEND;".into();
        }
    });
    let p = plan(Flavor::Oracle, Some(&pkg), &spec).unwrap();
    let kinds: Vec<_> = p.steps.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [StepKind::Replace, StepKind::Compile]);
    assert_eq!(
        p.steps[1].statements,
        ["ALTER PACKAGE BILLING.INVOICE_PKG COMPILE BODY"]
    );
    assert!(p.atomic && p.source_lines);
    let wrong = with(&pkg, |p| {
        if let Properties::Source(s) = p {
            s.spec = "TABLE x (a number)".into();
        }
    });
    assert!(plan(Flavor::Oracle, Some(&pkg), &wrong).is_err());
    let recompile = recompile_plan(Flavor::Oracle, &pkg.object).unwrap();
    assert_eq!(
        recompile.steps[0].statements,
        ["ALTER PACKAGE BILLING.INVOICE_PKG COMPILE"]
    );
    assert!(recompile_plan(Flavor::Mysql, &pkg.object).is_err());
}

#[test]
fn oracle_matviews_alter_refresh_and_swap_for_a_new_query() {
    let mut d = def(
        Flavor::Oracle,
        "BILLING",
        "MONTHLY_REVENUE_MV",
        ObjectKind::MaterializedView,
        Properties::MaterializedView(MatViewProps {
            query: "SELECT 1 n FROM dual".into(),
            ..MatViewProps::default()
        }),
    );
    d.grants = vec![Grant {
        user: "REPORTING".into(),
        host: String::new(),
        privileges: vec!["SELECT".into()],
        grant_option: false,
    }];
    let refresh = with(&d, |p| {
        if let Properties::MaterializedView(m) = p {
            m.start_with = "SYSDATE".into();
            m.next = "TRUNC(SYSDATE) + 1 + 2/24".into();
        }
    });
    let p = plan(Flavor::Oracle, Some(&d), &refresh).unwrap();
    assert_eq!(
        p.steps[0].statements,
        [
            "ALTER MATERIALIZED VIEW BILLING.MONTHLY_REVENUE_MV\nREFRESH FORCE ON DEMAND START WITH \
          SYSDATE NEXT TRUNC(SYSDATE) + 1 + 2/24"
        ]
    );
    let query = with(&d, |p| {
        if let Properties::MaterializedView(m) = p {
            m.query = "SELECT 2 n FROM dual".into();
        }
    });
    let p = plan(Flavor::Oracle, Some(&d), &query).unwrap();
    assert!(!p.atomic);
    assert_eq!(
        p.steps[0].statements[0],
        "DROP MATERIALIZED VIEW BILLING.MONTHLY_REVENUE_MV"
    );
    assert_eq!(
        p.steps[1].statements,
        ["GRANT SELECT ON BILLING.MONTHLY_REVENUE_MV TO REPORTING"]
    );
    assert_eq!(p.steps[2].kind, StepKind::Restore);
}

#[test]
fn synonyms_point_at_their_target() {
    let d = def(
        Flavor::Oracle,
        "BILLING",
        "INVOICES_REMOTE",
        ObjectKind::Synonym,
        Properties::Synonym(SynonymProps {
            target_schema: "FINANCE".into(),
            target: "INVOICES".into(),
            db_link: "FIN_LINK".into(),
            ..SynonymProps::default()
        }),
    );
    let p = plan(Flavor::Oracle, None, &d.draft()).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["CREATE SYNONYM BILLING.INVOICES_REMOTE FOR FINANCE.INVOICES@FIN_LINK"]
    );
    let public = with(&d, |p| {
        if let Properties::Synonym(s) = p {
            s.public = true;
            s.db_link.clear();
        }
    });
    let p = plan(Flavor::Oracle, Some(&d), &public).unwrap();
    assert_eq!(
        p.steps[0].statements,
        ["CREATE PUBLIC SYNONYM INVOICES_REMOTE FOR FINANCE.INVOICES"]
    );
    assert_eq!(
        p.steps[1].statements,
        ["DROP SYNONYM BILLING.INVOICES_REMOTE"]
    );
    assert_eq!(p.object.schema, "PUBLIC");
    assert!(!p.atomic);
    assert_eq!(
        drop_plan(Flavor::Oracle, &p.object).steps[0].statements,
        ["DROP PUBLIC SYNONYM INVOICES_REMOTE"]
    );
    let bad = with(&d, |p| {
        if let Properties::Synonym(s) = p {
            s.db_link = "x; drop".into();
        }
    });
    assert!(plan(Flavor::Oracle, Some(&d), &bad).is_err());
}

#[test]
fn names_are_quoted_and_fragments_checked() {
    assert_eq!(quote_name(Flavor::Mysql, "a`b"), "`a``b`");
    assert_eq!(quote_name(Flavor::Oracle, "ORDERS"), "ORDERS");
    assert_eq!(quote_name(Flavor::Oracle, "Orders"), "\"Orders\"");
    assert_eq!(quote_name(Flavor::Oracle, "ORDER"), "\"ORDER\"");
    assert_eq!(quote_name(Flavor::Oracle, "A\"B"), "\"A\"\"B\"");
    assert_eq!(normalize_name(Flavor::Oracle, " calc_fee "), "CALC_FEE");
    assert_eq!(normalize_name(Flavor::Oracle, "my fee"), "my fee");
    assert_eq!(normalize_name(Flavor::Mysql, "Calc"), "Calc");
    let mut r = restock(Flavor::Mysql);
    r.read_only = Some("no body".into());
    assert_eq!(
        plan(Flavor::Mysql, Some(&r), &r.draft()).unwrap_err(),
        "no body"
    );
    for bad in [
        "INT; DROP TABLE x",
        "INT -- x",
        "INT /* x */",
        "INT # x",
        "VARCHAR(10",
        "ENUM('a)",
    ] {
        let draft = with(&restock(Flavor::Mysql), |p| {
            if let Properties::Routine(r) = p {
                r.params[0].data_type = bad.into();
            }
        });
        assert!(plan(Flavor::Mysql, None, &draft).is_err(), "{bad}");
    }
    let ok = with(&restock(Flavor::Mysql), |p| {
        if let Properties::Routine(r) = p {
            r.params[0].data_type = "ENUM('a;b', 'c')".into();
            r.comment = "it's \\ fine".into();
        }
    });
    let p = plan(Flavor::Mysql, None, &ok).unwrap();
    assert!(p.steps[0].statements[0].contains("COMMENT 'it''s \\\\ fine'"));
    let evil = ObjectDraft {
        object: ObjectRef::new("s", "t`; DROP TABLE x; --", ObjectKind::View),
        props: Properties::View(ViewProps {
            query: "SELECT 1".into(),
            ..ViewProps::default()
        }),
    };
    let p = plan(Flavor::Mysql, None, &evil).unwrap();
    assert!(p.steps[0].statements[0].contains("`s`.`t``; DROP TABLE x; --`"));
}

#[test]
fn templates_say_what_is_missing() {
    let t = template(Flavor::Mysql, ObjectKind::Trigger, "warehouse");
    assert_eq!(
        plan(Flavor::Mysql, None, &t).unwrap_err(),
        "To finish: name, pick a table."
    );
    let mut t = t;
    t.object.name = "orders_bi".into();
    if let Properties::Trigger(tr) = &mut t.props {
        tr.table = "orders".into();
    }
    let p = plan(Flavor::Mysql, None, &t).unwrap();
    assert_eq!(p.verb(), "CREATE");
    assert!(p.previous.is_none() && p.atomic);
    for flavor in [Flavor::Mysql, Flavor::Mariadb, Flavor::Oracle] {
        for &kind in ObjectKind::managed(flavor) {
            let mut d = template(flavor, kind, "S");
            d.object.name = "n".into();
            if let Properties::Trigger(tr) = &mut d.props {
                tr.table = "T".into();
            }
            if let Properties::Synonym(s) = &mut d.props {
                s.target = "T".into();
            }
            let p = plan(flavor, None, &d);
            assert!(p.is_ok(), "{flavor:?} {kind:?}: {p:?}");
        }
    }
    let pkg = plan(
        Flavor::Oracle,
        None,
        &template(Flavor::Oracle, ObjectKind::Package, "APP"),
    )
    .unwrap();
    assert_eq!(pkg.object.name, "NEW_PACKAGE");
    assert_eq!(pkg.steps.len(), 2);
}

#[test]
fn nodes_name_their_objects() {
    let id = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        ObjectRef::from_node(&id(&["c", "app", "Triggers", "t1"])),
        Some(ObjectRef::new("app", "t1", ObjectKind::Trigger))
    );
    assert_eq!(
        ObjectRef::from_node(&id(&["c", "app", "Tables", "orders", "Triggers", "t1"])),
        Some(ObjectRef::new("app", "t1", ObjectKind::Trigger))
    );
    assert_eq!(
        ObjectRef::from_node(&id(&["c", "app", "Tables", "orders"])),
        None
    );
    assert_eq!(
        ObjectRef::from_node(&id(&["c", "APP", "Materialized views", "MV"])).map(|o| o.kind),
        Some(ObjectKind::MaterializedView)
    );
    assert!(ObjectKind::Trigger.can_rename(Flavor::Mysql).is_err());
    assert!(ObjectKind::Event.can_rename(Flavor::Mariadb).is_ok());
    assert!(ObjectKind::Synonym.can_rename(Flavor::Oracle).is_ok());
    assert!(ObjectKind::Package.can_rename(Flavor::Oracle).is_err());
    assert_eq!(
        ObjectKind::from_label("materialized view"),
        Some(ObjectKind::MaterializedView)
    );
}

#[test]
fn error_lines_map_onto_the_body() {
    let statement = "CREATE TRIGGER t\nBEFORE UPDATE ON x FOR EACH ROW\nBEGIN\n  IF a\n  THN\nEND";
    let body = "BEGIN\n  IF a\n  THN\nEND";
    assert_eq!(body_line(statement, body, 5), Some(3));
    assert_eq!(body_line(statement, body, 1), None);
    assert_eq!(body_line(statement, "", 5), None);
}

#[test]
fn backups_land_under_the_project() {
    let o = ObjectRef::new(
        "ware house",
        "orders_audit_bu",
        ObjectKind::MaterializedView,
    );
    let p = backup_path(Path::new("/p"), "prod/1", &o, "20261124-104200");
    assert_eq!(
        p,
        Path::new(
            "/p/.deadlock/backups/prod_1-ware_house-materialized_view-orders_audit_bu-20261124-104200.sql"
        )
    );
}

/// Picks the statements a [`Fake`] fails, by text and earlier runs of the same text.
type Fail = Box<dyn Fn(&str, usize) -> Option<&'static str> + Send>;

/// Runs statements in memory: fails those `fail` picks, with "denied" when asked.
struct Fake {
    ran: Vec<String>,
    fail: Fail,
}

impl Runner for Fake {
    fn run<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        let n = self.ran.iter().filter(|s| *s == sql).count();
        self.ran.push(sql.to_owned());
        let r = match (self.fail)(sql, n) {
            Some(e) => Err(DriverError::Invalid(e.into())),
            None => Ok(()),
        };
        Box::pin(std::future::ready(r))
    }

    fn denied(&self, e: &DriverError) -> bool {
        e.to_string().contains("denied")
    }
}

fn run(
    plan: &ChangePlan,
    fail: impl Fn(&str, usize) -> Option<&'static str> + Send + 'static,
    backup: Option<&Backup>,
) -> (ApplyOutcome, Vec<String>) {
    let rt = crate::driver::Runtime::new().unwrap();
    let mut fake = Fake {
        ran: Vec::new(),
        fail: Box::new(fail),
    };
    let out = rt.block_on(execute(plan, &mut fake, backup));
    (out, fake.ran)
}

fn swap_plan() -> ChangePlan {
    let d = restock(Flavor::Mysql);
    plan(
        Flavor::Mysql,
        Some(&d),
        &with(&d, |p| {
            if let Properties::Routine(r) = p {
                r.body = "BEGIN\n  SELECT 3;\nEND".into();
            }
        }),
    )
    .unwrap()
}

#[test]
fn a_swap_that_works_runs_everything_but_the_restore() {
    let p = swap_plan();
    let (out, ran) = run(&p, |_, _| None, None);
    assert_eq!(out.state, FinalState::Changed);
    assert_eq!(ran.len(), 5); // check, cleanup, drop, create, grant
    assert!(ran[1].starts_with("DROP PROCEDURE `billing`.`_dl_check_"));
    let st: Vec<_> = out.steps.iter().map(|s| s.status).collect();
    assert_eq!(
        st,
        [
            StepStatus::Done,
            StepStatus::Done,
            StepStatus::Done,
            StepStatus::NotRun,
            StepStatus::NotRun
        ]
    );
    assert_eq!(out.refresh, [ObjectPath(vec!["billing".into()])]);
}

#[test]
fn a_failed_check_changes_nothing_and_cleans_up_only_what_it_made() {
    let p = swap_plan();
    let (out, ran) = run(
        &p,
        |s, _| s.contains("_dl_check_").then_some("syntax error at line 9"),
        None,
    );
    assert_eq!(out.state, FinalState::Unchanged);
    // The temporary routine was never made, so nothing is dropped.
    assert_eq!(ran.len(), 1);
    assert_eq!(out.errors[0].line, None);
    let d = audit_trigger(Flavor::Mysql);
    let t = plan(Flavor::Mysql, Some(&d), &with(&d, new_body)).unwrap();
    let (out, ran) = run(
        &t,
        |s, _| s.starts_with("CREATE DEFINER").then_some("bad"),
        None,
    );
    assert_eq!(out.state, FinalState::Unchanged);
    assert_eq!(
        ran.last().unwrap(),
        "DROP TABLE `warehouse`.`_dl_check_orders`"
    );
    let (out, ran) = run(
        &t,
        |s, _| {
            s.starts_with("CREATE TABLE")
                .then_some("CREATE command denied")
        },
        None,
    );
    assert_eq!(out.state, FinalState::Changed);
    assert_eq!(out.steps[0].status, StepStatus::Skipped);
    assert!(out.notices[0].starts_with("The check was skipped"));
    assert!(!ran.iter().any(|s| s.starts_with("DROP TABLE")));
}

#[test]
fn a_failed_create_restores_the_original_and_its_grants() {
    let p = swap_plan();
    let (out, ran) = run(
        &p,
        |s, _| {
            s.starts_with("CREATE DEFINER=`billing_app`@`%` PROCEDURE `billing`.`restock_product`")
                .then_some("Lock wait timeout exceeded")
        },
        None,
    );
    assert_eq!(out.state, FinalState::Restored);
    assert_eq!(
        &ran[ran.len() - 2..],
        [
            "CREATE PROCEDURE original",
            "GRANT EXECUTE ON PROCEDURE `billing`.`restock_product` TO 'app'@'%'"
        ]
    );
    let st: Vec<_> = out.steps.iter().map(|s| s.status).collect();
    assert_eq!(
        st,
        [
            StepStatus::Done,
            StepStatus::Failed,
            StepStatus::NotRun,
            StepStatus::Done,
            StepStatus::Done
        ]
    );
    assert!(out.errors[0].message.contains("Lock wait"));
}

#[test]
fn a_failed_restore_writes_the_backup() {
    let dir = tempfile::tempdir().unwrap();
    let backup = Backup {
        root: dir.path().to_owned(),
        connection: "prod".into(),
    };
    let p = swap_plan();
    let (out, _) = run(
        &p,
        |s, _| (s.starts_with("CREATE") && !s.contains("_dl_check_")).then_some("connection lost"),
        Some(&backup),
    );
    let FinalState::Missing { backup: text, path } = &out.state else {
        panic!("{:?}", out.state)
    };
    assert_eq!(text, "CREATE PROCEDURE original;\n");
    let path = path.as_ref().unwrap();
    assert!(path.starts_with(dir.path().join(".deadlock/backups")));
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("prod-billing-procedure-restock_product-")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), *text);
    assert_eq!(out.errors.len(), 2);
}
