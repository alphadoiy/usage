use super::*;
use std::fs;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("usage-run-evidence-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn db(&self) -> Connection {
        Connection::open(self.0.join("manager.sqlite3")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn at(second: u32) -> i64 {
    timestamp(&format!("2026-10-18T00:00:{second:02}Z")).unwrap()
}

#[test]
fn account_adapter_returns_typed_lifetimes_and_supports_legacy_missing_turns() {
    let fixture = Fixture::new();
    let db = fixture.db();
    db.execute_batch(
        "CREATE TABLE runs(id TEXT PRIMARY KEY,profile_id TEXT,revision INTEGER,started_at TEXT,finished_at TEXT);
        CREATE TABLE run_sessions(run_id TEXT,session_id TEXT,first_seen TEXT,last_seen TEXT);
        CREATE INDEX session_lookup ON run_sessions(session_id,last_seen);
        INSERT INTO runs VALUES
          ('A','account-A',2,'2026-10-18T00:00:02Z','2026-10-18T00:00:20Z'),
          ('B','account-B',3,'2026-10-18T00:00:00Z',NULL),
          ('invalid','account-A',2,'broken',NULL),
          ('outside','account-A',2,'2026-10-18T00:00:30Z',NULL);
        INSERT INTO run_sessions VALUES
          ('A','shared','2026-10-18T00:00:05.000Z','2026-10-18T00:00:30Z'),
          ('B','shared','2026-10-18T00:00:10Z','2026-10-18T00:00:25.000Z'),
          ('invalid','shared','2026-10-18T00:00:10Z','2026-10-18T00:00:25Z'),
          ('outside','shared','2026-10-18T00:00:30Z','2026-10-18T00:00:40Z');",
    )
    .unwrap();
    let source = source(&fixture.0);
    let sessions = vec!["shared".into()];
    let query = SessionEvidenceQuery {
        sessions: &sessions,
        since_ms: at(0),
        until_ms: at(22),
    };
    let before = fs::read(fixture.0.join("manager.sqlite3")).unwrap();
    let evidence = source.read(query).unwrap();
    assert_eq!(
        fs::read(fixture.0.join("manager.sqlite3")).unwrap(),
        before,
        "reading old schemas does not create or migrate account tables"
    );
    let mut claims = evidence.sessions["shared"].clone();
    claims.sort_by(|a, b| a.run.cmp(&b.run));
    assert_eq!(
        claims.len(),
        2,
        "invalid and future lifetimes provide no claim"
    );
    assert_eq!(
        (
            &*claims[0].account,
            claims[0].revision,
            claims[0].start,
            claims[0].end
        ),
        ("account-A", 2, at(5), at(20))
    );
    assert_eq!(
        (
            &*claims[1].account,
            claims[1].revision,
            claims[1].start,
            claims[1].end
        ),
        ("account-B", 3, at(10), at(22))
    );
    assert!(claims.iter().all(|claim| claim.turns.is_empty()));

    db.execute_batch(
        "CREATE TABLE run_events(run_id TEXT,session_id TEXT,turn_id TEXT,observed_at TEXT);
        CREATE INDEX event_lookup ON run_events(session_id,observed_at);
        INSERT INTO run_events VALUES
          ('A','shared','turn-A','2026-10-18T00:00:15Z'),
          ('B','shared','turn-B','2026-10-18T00:00:18.000Z'),
          ('A','unrequested','other-session','2026-10-18T00:00:15Z');",
    )
    .unwrap();
    let current = source.read(query).unwrap();
    assert_eq!(current.sessions.len(), 1);
    assert!(
        current.sessions["shared"]
            .iter()
            .all(|claim| { claim.turns == HashSet::from([format!("turn-{}", claim.run)]) })
    );
    assert!(
        evidence.sessions["shared"]
            .iter()
            .all(|claim| claim.turns.is_empty()),
        "returned snapshots remain immutable after the next event is saved"
    );
}

#[test]
fn missing_history_is_not_created_and_request_budget_is_enforced_before_reading() {
    let fixture = Fixture::new();
    let missing = fixture.0.join("missing-store");
    let source = source(&missing);
    let sessions = vec!["shared".into()];
    let query = SessionEvidenceQuery {
        sessions: &sessions,
        since_ms: at(0),
        until_ms: at(30),
    };
    assert!(source.read(query).unwrap().sessions.is_empty());
    assert!(!missing.exists());
    let too_many = vec!["session".into(); crate::runs::MAX_EVIDENCE_SESSIONS + 1];
    assert!(
        source
            .read(SessionEvidenceQuery {
                sessions: &too_many,
                ..query
            })
            .unwrap_err()
            .contains("范围过大")
    );
    assert!(
        source
            .read(SessionEvidenceQuery {
                since_ms: at(31),
                ..query
            })
            .is_err()
    );
    assert!(!missing.exists());
}
