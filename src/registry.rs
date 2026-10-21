//! ECS flight / config cache reset in Office's MicrosoftRegistrationDB.reg.
//!
//! Mirrors the registry block of the 2.7.1-local postinstall: same node walk,
//! same value names, same order. All SQL is parameterised.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

pub const APPS: [&str; 6] = [
    "word",
    "excel",
    "powerpoint",
    "outlook",
    "onenote",
    "licensingdaemon",
];
const FLIGHT_VALUES: [&str; 4] = ["ETag", "Expires", "DeferredConfigs", "ConfigIds"];
const NUKE_VALUES: [&str; 3] = ["AuthorityUrl", "FilePath", "Url"];

/// One DELETE the script would issue against HKEY_CURRENT_USER_values.
#[derive(Debug, PartialEq)]
enum Delete {
    /// One named value under one node (script: DeleteValue).
    Value { name: &'static str, node_id: i64 },
    /// Every value under one node (script: DeleteNode).
    Node(i64),
    /// Every value with this name, any node (script: NukeValue).
    Named(&'static str),
}

impl Delete {
    fn select_sql(&self) -> &'static str {
        match self {
            Delete::Value { .. } => {
                "SELECT rowid FROM HKEY_CURRENT_USER_values WHERE name = ?1 AND node_id = ?2"
            }
            Delete::Node(_) => "SELECT rowid FROM HKEY_CURRENT_USER_values WHERE node_id = ?1",
            Delete::Named(_) => "SELECT rowid FROM HKEY_CURRENT_USER_values WHERE name = ?1",
        }
    }

    fn delete_sql(&self) -> &'static str {
        match self {
            Delete::Value { .. } => {
                "DELETE FROM HKEY_CURRENT_USER_values WHERE name = ?1 AND node_id = ?2"
            }
            Delete::Node(_) => "DELETE FROM HKEY_CURRENT_USER_values WHERE node_id = ?1",
            Delete::Named(_) => "DELETE FROM HKEY_CURRENT_USER_values WHERE name = ?1",
        }
    }

    fn params(&self) -> Vec<rusqlite::types::Value> {
        use rusqlite::types::Value as V;
        match *self {
            Delete::Value { name, node_id } => vec![V::Text(name.into()), V::Integer(node_id)],
            Delete::Node(node_id) => vec![V::Integer(node_id)],
            Delete::Named(name) => vec![V::Text(name.into())],
        }
    }
}

/// Look up a child node; a missing parent or child yields None (script: GetNodeId).
fn node_id(conn: &Connection, name: &str, parent: Option<i64>) -> rusqlite::Result<Option<i64>> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT node_id FROM HKEY_CURRENT_USER WHERE name = ?1 AND parent_id = ?2",
        params![name, parent],
        |r| r.get(0),
    )
    .optional()
}

/// Walk the tree and list the deletes in the script's order.
fn plan(conn: &Connection) -> rusqlite::Result<Vec<Delete>> {
    let software = node_id(conn, "Software", Some(-1))?;
    let microsoft = node_id(conn, "Microsoft", software)?;
    let office = node_id(conn, "Office", microsoft)?;
    let version = node_id(conn, "16.0", office)?;
    let common = node_id(conn, "Common", version)?;
    let ecs = node_id(conn, "ExperimentEcs", common)?;
    let ex_configs = node_id(conn, "ExperimentConfigs", common)?;
    let ex_ecs = node_id(conn, "Ecs", ex_configs)?;

    let mut out = Vec::new();
    for app in APPS {
        let key_app = node_id(conn, app, ecs)?;
        let key_app_flights = node_id(conn, "Flights", key_app)?;
        let key_ex_app = node_id(conn, app, ex_ecs)?;
        let key_ex_app_ccd = node_id(conn, "ConfigContextData", key_ex_app)?;
        for node in [key_app, key_ex_app].into_iter().flatten() {
            for name in FLIGHT_VALUES {
                out.push(Delete::Value {
                    name,
                    node_id: node,
                });
            }
        }
        for node in [key_app_flights, key_ex_app_ccd].into_iter().flatten() {
            out.push(Delete::Node(node));
        }
    }
    for name in NUKE_VALUES {
        out.push(Delete::Named(name));
    }
    Ok(out)
}

/// Read-only: number of distinct value rows `delete_flights` would remove.
pub fn count_flights(db: &Path) -> rusqlite::Result<usize> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", true)?;
    let mut rows = BTreeSet::new();
    for d in plan(&conn)? {
        let mut stmt = conn.prepare(d.select_sql())?;
        let ids = stmt.query_map(rusqlite::params_from_iter(d.params()), |r| {
            r.get::<_, i64>(0)
        })?;
        for id in ids {
            rows.insert(id?);
        }
    }
    Ok(rows.len())
}

/// Delete the flight / config rows in one transaction; returns rows affected.
pub fn delete_flights(db: &Path) -> rusqlite::Result<usize> {
    let mut conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    // Immediate: take the write lock before the walk, so Office cannot change
    // the tree between the node lookups and the deletes.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut total = 0;
    for d in plan(&tx)? {
        total += tx.execute(d.delete_sql(), rusqlite::params_from_iter(d.params()))?;
    }
    tx.commit()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SCHEMA: &str = "
        CREATE TABLE HKEY_CURRENT_USER (node_id INTEGER PRIMARY KEY ASC, parent_id, name TEXT COLLATE nocase, write_time, UNIQUE (parent_id, name));
        CREATE TABLE HKEY_CURRENT_USER_values (node_id INTEGER, name TEXT COLLATE nocase, type INTEGER, value, UNIQUE (node_id, name));
    ";

    fn new_db(tag: &str) -> PathBuf {
        // keep(): fixtures are left on disk (no deletes in this workspace).
        let dir = tempfile::Builder::new()
            .prefix(&format!("olr-unit-{tag}-"))
            .tempdir()
            .unwrap()
            .keep();
        let db = dir.join("MicrosoftRegistrationDB.reg");
        Connection::open(&db)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        db
    }

    fn node(c: &Connection, id: i64, parent: i64, name: &str) {
        c.execute(
            "INSERT INTO HKEY_CURRENT_USER (node_id, parent_id, name, write_time) VALUES (?1, ?2, ?3, 0)",
            params![id, parent, name],
        )
        .unwrap();
    }

    fn val(c: &Connection, node_id: i64, name: &str) {
        c.execute(
            "INSERT INTO HKEY_CURRENT_USER_values (node_id, name, type, value) VALUES (?1, ?2, 1, 'x')",
            params![node_id, name],
        )
        .unwrap();
    }

    fn total(c: &Connection) -> i64 {
        c.query_row("SELECT COUNT(*) FROM HKEY_CURRENT_USER_values", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn has(c: &Connection, node_id: i64, name: &str) -> bool {
        c.query_row(
            "SELECT COUNT(*) FROM HKEY_CURRENT_USER_values WHERE node_id = ?1 AND name = ?2",
            params![node_id, name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    /// 25 value rows: 19 the walk must delete, 6 that must survive.
    fn seed(db: &Path) {
        let c = Connection::open(db).unwrap();
        node(&c, 1, -1, "Software");
        node(&c, 2, 1, "Microsoft");
        node(&c, 3, 2, "Office");
        node(&c, 4, 3, "16.0");
        node(&c, 5, 4, "Common");
        node(&c, 6, 5, "ExperimentEcs");
        node(&c, 10, 6, "word");
        node(&c, 11, 10, "Flights");
        node(&c, 12, 6, "excel");
        node(&c, 13, 6, "licensingdaemon");
        node(&c, 14, 13, "Flights");
        node(&c, 15, 6, "otherapp");
        node(&c, 16, 15, "Flights");
        node(&c, 20, 5, "ExperimentConfigs");
        node(&c, 21, 20, "Ecs");
        node(&c, 22, 21, "Outlook"); // mixed case: name is COLLATE nocase
        node(&c, 23, 22, "ConfigContextData");
        node(&c, 24, 21, "onenote");
        node(&c, 30, 5, "Unrelated");

        // Deleted (19).
        for v in ["ETag", "Expires", "DeferredConfigs", "ConfigIds"] {
            val(&c, 10, v);
        }
        for v in ["f1", "f2", "f3", "Url"] {
            val(&c, 11, v); // Url matched twice (Flights + nuke), counted once
        }
        val(&c, 12, "ETag");
        val(&c, 13, "ConfigIds");
        val(&c, 14, "x");
        val(&c, 22, "ETag");
        val(&c, 22, "Expires");
        val(&c, 23, "c1");
        val(&c, 23, "c2");
        val(&c, 24, "DeferredConfigs");
        val(&c, 30, "AuthorityUrl");
        val(&c, 30, "url"); // nocase
        val(&c, 30, "FilePath");

        // Survivors (6).
        val(&c, 10, "KeepMe");
        val(&c, 15, "ETag"); // app not in the list
        val(&c, 16, "y"); // Flights of an app not in the list
        val(&c, 22, "Other");
        val(&c, 30, "UrlKeep");
        val(&c, 30, "ETag"); // ETag outside an app node
    }

    #[test]
    fn walk_deletes_exact_rows_and_keeps_others() {
        let db = new_db("walk");
        seed(&db);
        let c = Connection::open(&db).unwrap();
        assert_eq!(total(&c), 25);

        assert_eq!(count_flights(&db).unwrap(), 19);
        assert_eq!(total(&c), 25, "count must not modify the DB");

        assert_eq!(delete_flights(&db).unwrap(), 19);
        assert_eq!(total(&c), 6);
        for (n, v) in [
            (10, "KeepMe"),
            (15, "ETag"),
            (16, "y"),
            (22, "Other"),
            (30, "UrlKeep"),
            (30, "ETag"),
        ] {
            assert!(has(&c, n, v), "survivor {n}/{v} missing");
        }
        // Nodes themselves are never deleted.
        let nodes: i64 = c
            .query_row("SELECT COUNT(*) FROM HKEY_CURRENT_USER", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nodes, 19);

        // Second run is a no-op.
        assert_eq!(count_flights(&db).unwrap(), 0);
        assert_eq!(delete_flights(&db).unwrap(), 0);
    }

    #[test]
    fn missing_nodes_are_skipped() {
        let db = new_db("missing");
        let c = Connection::open(&db).unwrap();
        // Tree stops at Common: no ExperimentEcs / ExperimentConfigs.
        node(&c, 1, -1, "Software");
        node(&c, 2, 1, "Microsoft");
        node(&c, 3, 2, "Office");
        node(&c, 4, 3, "16.0");
        node(&c, 5, 4, "Common");
        node(&c, 7, 5, "word"); // app name directly under Common: not walked
                                // "Software" under a non-root parent must not be treated as the root.
        node(&c, 50, 99, "Software");
        node(&c, 51, 50, "Microsoft");
        val(&c, 7, "ETag");
        val(&c, 51, "ETag");
        val(&c, 7, "FilePath"); // nuke still applies
        assert_eq!(total(&c), 3);

        assert_eq!(count_flights(&db).unwrap(), 1);
        assert_eq!(delete_flights(&db).unwrap(), 1);
        assert_eq!(total(&c), 2);
        assert!(has(&c, 7, "ETag"));
        assert!(has(&c, 51, "ETag"));
    }

    #[test]
    fn plan_matches_script_order() {
        let db = new_db("order");
        seed(&db);
        let c = Connection::open(&db).unwrap();
        let p = plan(&c).unwrap();
        // word: 4 values on ExperimentEcs/word, Flights node; no ExperimentConfigs/word.
        assert_eq!(
            p[..5],
            [
                Delete::Value {
                    name: "ETag",
                    node_id: 10
                },
                Delete::Value {
                    name: "Expires",
                    node_id: 10
                },
                Delete::Value {
                    name: "DeferredConfigs",
                    node_id: 10
                },
                Delete::Value {
                    name: "ConfigIds",
                    node_id: 10
                },
                Delete::Node(11),
            ]
        );
        assert_eq!(
            p[p.len() - 3..],
            [
                Delete::Named("AuthorityUrl"),
                Delete::Named("FilePath"),
                Delete::Named("Url")
            ]
        );
    }

    #[test]
    fn missing_db_is_an_error_not_created() {
        let dir = tempfile::Builder::new()
            .prefix("olr-unit-nodb-")
            .tempdir()
            .unwrap()
            .keep();
        let db = dir.join("MicrosoftRegistrationDB.reg");
        assert!(count_flights(&db).is_err());
        assert!(delete_flights(&db).is_err());
        assert!(!db.exists());
    }
}
