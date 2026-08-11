use sqlx::{Connection, PgConnection};

use super::super::*;

pub(super) struct RunFenceLock {
    connection: PgConnection,
    lock_name: String,
    pub(super) backend_pid: i32,
}

impl RunFenceLock {
    pub(super) async fn acquire(pair: &ProcessPair) -> Self {
        let lock_name = format!("ironcrew:{}idempotency:run-fence:6:global", pair.prefix);
        let mut connection = PgConnection::connect(&pair.database_url)
            .await
            .expect("connect IC-020 run-fence blocker");
        let backend_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .expect("read IC-020 blocker backend pid");
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(&lock_name)
            .execute(&mut connection)
            .await
            .expect("hold IC-020 global run-fence lock");
        Self {
            connection,
            lock_name,
            backend_pid,
        }
    }

    pub(super) async fn release(mut self) {
        let released: bool =
            sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
                .bind(&self.lock_name)
                .fetch_one(&mut self.connection)
                .await
                .expect("release IC-020 global run-fence lock");
        assert!(released, "IC-020 run-fence lock was not held");
        self.connection
            .close()
            .await
            .expect("close IC-020 blocker connection");
    }
}
