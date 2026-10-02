// Postgres: the news / RAG store and unified memory.
//
// Sections:
//   1. pool    — connection pool built once in the composition root
//   2. migrate — a byte-compatible port of drizzle-orm's pg migrator
//   3. vectors — pgvector values as text literals
//   4. query   — a tiny builder for statements with optional filters
//
// The migrator matters most. The TS server migrated this database with
// drizzle, which records each applied file in `drizzle.__drizzle_migrations`
// keyed by the journal's `when` timestamp. Reading and writing that same
// table, with the same hashes, is what lets the Rust and TS servers take
// turns on one database during the switch-over without re-running DDL.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;

// ── 1. pool ──────────────────────────────────────────────────────────────────

pub type PgPool = Pool;

pub fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let config: tokio_postgres::Config = database_url.parse()?;
    let manager = Manager::from_config(config, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
    Ok(Pool::builder(manager).max_size(10).build()?)
}

pub fn database_url() -> anyhow::Result<String> {
    std::env::var("DATABASE_URL").ok().filter(|v| !v.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "DATABASE_URL is not set. The mcp container needs Postgres for the news/RAG store; see .env.postgres.example."
        )
    })
}

// ── 2. migrate ───────────────────────────────────────────────────────────────

struct Migration {
    // Journal `when` (ms) — drizzle's ordering key and the stored created_at.
    when: i64,
    sql: &'static str,
}

// packages/mcp/src/db/pg/migrations/meta/_journal.json, in order. The SQL
// files are byte-identical copies: their sha256 is the stored hash.
const MIGRATIONS: &[Migration] = &[
    Migration { when: 1780238110688, sql: include_str!("../migrations/pg/0000_init_news.sql") },
    Migration { when: 1780326897486, sql: include_str!("../migrations/pg/0001_fancy_omega_red.sql") },
    Migration { when: 1781001341744, sql: include_str!("../migrations/pg/0002_wooden_anthem.sql") },
    Migration { when: 1787496077308, sql: include_str!("../migrations/pg/0003_lazy_puppet_master.sql") },
];

const BREAKPOINT: &str = "--> statement-breakpoint";

// `mcp` and `mcp-tunnel` boot against one database at the same moment;
// unserialised, both would race through the same DDL. A session advisory
// lock makes the second wait and then find nothing left to apply.
const MIGRATION_LOCK: i64 = 0x006d_6370_5f6d_6967; // "mcp_mig"

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let mut client = pool.get().await?;
    client.execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK]).await?;
    let result = migrate_locked(&mut client).await;
    client.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK]).await?;
    result
}

async fn migrate_locked(client: &mut deadpool_postgres::Object) -> anyhow::Result<()> {
    // Outside the migration files so they never assume CREATE EXTENSION rights.
    client.batch_execute("CREATE EXTENSION IF NOT EXISTS vector").await?;
    client
        .batch_execute(
            r#"CREATE SCHEMA IF NOT EXISTS "drizzle";
               CREATE TABLE IF NOT EXISTS "drizzle"."__drizzle_migrations" (
                 id SERIAL PRIMARY KEY,
                 hash text NOT NULL,
                 created_at bigint
               )"#,
        )
        .await?;
    let last: Option<i64> = client
        .query_opt(r#"SELECT created_at FROM "drizzle"."__drizzle_migrations" ORDER BY created_at DESC LIMIT 1"#, &[])
        .await?
        .and_then(|row| row.get(0));

    let tx = client.transaction().await?;
    let mut applied = 0;
    for migration in MIGRATIONS.iter().filter(|m| last.is_none_or(|last| last < m.when)) {
        for statement in migration.sql.split(BREAKPOINT) {
            if !statement.trim().is_empty() {
                tx.batch_execute(statement).await?;
            }
        }
        let hash = hex::encode(Sha256::digest(migration.sql.as_bytes()));
        tx.execute(
            r#"INSERT INTO "drizzle"."__drizzle_migrations" ("hash", "created_at") VALUES ($1, $2)"#,
            &[&hash, &migration.when],
        )
        .await?;
        applied += 1;
    }
    tx.commit().await?;
    tracing::info!(applied, "pg migrations applied, store ready");
    Ok(())
}

// ── 3. vectors ───────────────────────────────────────────────────────────────

// pgvector accepts and prints `[1,2,3]`; passing it as text with a `::vector`
// cast keeps the driver free of a pgvector-specific type binding.
pub fn vector_literal(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 12 + 2);
    out.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&x.to_string());
    }
    out.push(']');
    out
}

pub fn parse_vector(s: &str) -> Option<Vec<f32>> {
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|x| x.trim().parse().ok()).collect()
}

// ── 4. query ─────────────────────────────────────────────────────────────────

pub type Param = Box<dyn tokio_postgres::types::ToSql + Sync + Send>;

// Accumulates bind parameters for a statement whose WHERE clause depends on
// which filters the caller set. `bind` returns the `$n` placeholder.
#[derive(Default)]
pub struct Query {
    params: Vec<Param>,
}

impl Query {
    pub fn bind(&mut self, value: impl tokio_postgres::types::ToSql + Sync + Send + 'static) -> String {
        self.params.push(Box::new(value));
        format!("${}", self.params.len())
    }

    pub fn params(&self) -> Vec<&(dyn tokio_postgres::types::ToSql + Sync)> {
        self.params.iter().map(|p| p.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync)).collect()
    }
}

pub fn where_clause(filters: &[String]) -> String {
    if filters.is_empty() { String::new() } else { format!("WHERE {}", filters.join(" AND ")) }
}

// Postgres-backed tests run only against a throwaway database:
//   TEST_DATABASE_URL=postgres://… cargo test
// Each test that needs one calls `test_pool()` and returns early without it.
#[cfg(test)]
pub async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("TEST_DATABASE_URL must parse");
    migrate(&pool).await.expect("migrations apply");
    Some(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrating_again_applies_nothing_and_keeps_drizzles_journal() {
        let Some(pool) = test_pool().await else { return };
        migrate(&pool).await.unwrap();
        let client = pool.get().await.unwrap();
        let rows = client
            .query(r#"SELECT hash, created_at FROM "drizzle"."__drizzle_migrations" ORDER BY id"#, &[])
            .await
            .unwrap();
        let journal: Vec<(String, i64)> =
            rows.iter().map(|r| (r.get::<_, String>(0)[..16].to_owned(), r.get(1))).collect();
        assert_eq!(
            journal,
            vec![
                ("6fe80692cd40ace1".to_owned(), 1780238110688),
                ("5829fed5a52485b9".to_owned(), 1780326897486),
                ("2d868dbf377baa83".to_owned(), 1781001341744),
                ("563cc5c374b93cd7".to_owned(), 1787496077308),
            ]
        );
    }

    #[test]
    fn migration_files_hash_like_drizzle_recorded_them() {
        // The hashes the TS migrator stored. A changed byte here would make
        // drizzle and this migrator disagree about what has been applied.
        let hashes: Vec<String> =
            MIGRATIONS.iter().map(|m| hex::encode(Sha256::digest(m.sql.as_bytes()))[..16].to_owned()).collect();
        assert_eq!(hashes, ["6fe80692cd40ace1", "5829fed5a52485b9", "2d868dbf377baa83", "563cc5c374b93cd7"]);
        assert!(MIGRATIONS.windows(2).all(|w| w[0].when < w[1].when));
    }

    #[test]
    fn vectors_round_trip_through_text() {
        let v = vec![0.5, -1.25, 3.0];
        assert_eq!(vector_literal(&v), "[0.5,-1.25,3]");
        assert_eq!(parse_vector("[0.5,-1.25,3]"), Some(v));
        assert_eq!(parse_vector("nope"), None);
    }
}
