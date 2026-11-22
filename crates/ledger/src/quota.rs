//! Identity-scoped observations coexist with the unattributed legacy history.

use rusqlite::{params, OptionalExtension};

use super::{
    new_cycle, Ledger, QuotaPointRow, Sample, QUOTA_HIST_GAP_SECS, QUOTA_HIST_KEPT_SECS,
    QUOTA_HIST_MAX,
};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS quota_observations (
        provider TEXT NOT NULL, subject TEXT NOT NULL, window TEXT NOT NULL,
        at INTEGER NOT NULL, left REAL NOT NULL, resets_at INTEGER,
        span_secs INTEGER NOT NULL,
        PRIMARY KEY (provider, subject, window, at)
    );";

impl Ledger {
    pub fn note_quota_window(
        &self,
        provider: &str,
        subject: &str,
        window: &manifest::quota::QuotaWindow,
    ) -> rusqlite::Result<()> {
        if subject.is_empty() {
            return Err(rusqlite::Error::InvalidParameterName(
                "quota subject is required".into(),
            ));
        }
        let now = window.observed_at;
        let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
        let next = Sample {
            at: now,
            left,
            resets_at: window.resets_at,
        };
        let tx = self.conn.unchecked_transaction()?;
        let last: Option<Sample> = tx
            .query_row(
                "SELECT at,left,resets_at FROM quota_observations
             WHERE provider=?1 AND subject=?2 AND window=?3 ORDER BY at DESC LIMIT 1",
                params![provider, subject, window.id],
                |row| {
                    Ok(Sample {
                        at: row.get(0)?,
                        left: row.get(1)?,
                        resets_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        if let Some(last) = last {
            if now < last.at {
                return Ok(());
            }
            if !new_cycle(&last, &next)
                && ((left - last.left).abs() < 0.05 || now - last.at < QUOTA_HIST_GAP_SECS)
            {
                tx.execute("DELETE FROM quota_observations WHERE provider=?1 AND subject=?2 AND window=?3 AND at=?4",
                    params![provider,subject,window.id,last.at])?;
            }
        }
        tx.execute(
            "INSERT INTO quota_observations(provider,subject,window,at,left,resets_at,span_secs)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(provider,subject,window,at) DO UPDATE SET
                left=excluded.left,resets_at=excluded.resets_at,span_secs=excluded.span_secs",
            params![
                provider,
                subject,
                window.id,
                now,
                left,
                window.resets_at,
                window.span_secs
            ],
        )?;
        tx.execute(
            "DELETE FROM quota_observations WHERE at < ?1",
            [now.saturating_sub(QUOTA_HIST_KEPT_SECS)],
        )?;
        tx.execute(
            "DELETE FROM quota_observations WHERE rowid IN (
                SELECT rowid FROM (SELECT rowid,ROW_NUMBER() OVER (
                    PARTITION BY provider,subject,window ORDER BY at DESC) AS rank
                    FROM quota_observations) WHERE rank > ?1)",
            [QUOTA_HIST_MAX],
        )?;
        tx.commit()
    }

    pub fn quota_subject_history(
        &self,
        provider: &str,
        subject: &str,
        since: u64,
    ) -> rusqlite::Result<Vec<QuotaPointRow>> {
        let mut statement = self.conn.prepare(
            "SELECT p.provider,p.window,p.at,p.left,p.resets_at,p.span_secs
             FROM quota_observations p
             WHERE p.provider=?1 AND p.subject=?2 AND (p.at>=?3 OR
                p.at=(SELECT MAX(q.at) FROM quota_observations q
                    WHERE q.provider=p.provider AND q.subject=p.subject
                        AND q.window=p.window AND q.at<?3))
             ORDER BY p.window,p.at",
        )?;
        let rows = statement
            .query_map(params![provider, subject, since], |row| {
                Ok(QuotaPointRow {
                    provider: row.get(0)?,
                    window: row.get(1)?,
                    at: row.get(2)?,
                    left: row.get(3)?,
                    resets_at: row.get(4)?,
                    span_secs: Some(row.get(5)?),
                    legacy: false,
                })
            })?
            .collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::quota::{ObservationSource, QuotaWindow};

    #[test]
    fn subjects_and_legacy_history_are_never_joined() {
        let ledger = Ledger::open(":memory:").unwrap();
        ledger
            .note_quota_point("p", "5h", 99.0, Some(5000), 1000)
            .unwrap();
        for (subject, used) in [("one", 20.0), ("two", 80.0)] {
            let window =
                QuotaWindow::new(18_000, used, Some(5000), 1000, 1, ObservationSource::Api)
                    .unwrap();
            ledger.note_quota_window("p", subject, &window).unwrap();
        }
        let one = ledger.quota_subject_history("p", "one", 0).unwrap();
        let two = ledger.quota_subject_history("p", "two", 0).unwrap();
        let legacy = ledger.quota_history(Some("p"), 0).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(two.len(), 1);
        assert_eq!(one[0].left, 80.0);
        assert_eq!(two[0].left, 20.0);
        assert_eq!(legacy[0].left, 99.0);
        assert!(!one[0].legacy);
        assert!(legacy[0].legacy);
        assert!(ledger
            .quota_subject_history("p", "unknown", 0)
            .unwrap()
            .is_empty());
        // A late response cannot erase the newer point even under the same identity.
        let old =
            QuotaWindow::new(18_000, 1.0, Some(5000), 999, 2, ObservationSource::Api).unwrap();
        ledger.note_quota_window("p", "one", &old).unwrap();
        assert_eq!(
            ledger.quota_subject_history("p", "one", 0).unwrap()[0].left,
            80.0
        );
    }

    #[test]
    fn opening_an_old_database_preserves_unattributed_points() {
        let root = std::env::temp_dir().join(format!(
            "aubridge-quota-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE quota_points(provider TEXT,window TEXT,at INTEGER,left REAL,resets_at INTEGER,PRIMARY KEY(provider,window,at));
            INSERT INTO quota_points VALUES('old','7d',1000,42,9000);").unwrap();
        drop(connection);
        let ledger = Ledger::open(&path).unwrap();
        let rows = ledger.quota_history(Some("old"), 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left, 42.0);
        assert!(rows[0].legacy);
        drop(ledger);
        std::fs::remove_dir_all(root).unwrap();
    }
}
