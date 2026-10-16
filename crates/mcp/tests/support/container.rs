// The Postgres the tests run against when TEST_DATABASE_URL names none: ONE
// pgvector container, found by name and shared by every test process and
// every run. The Rust testcontainers has no reaper, and a container held in a
// static is never dropped, so a container per test process outlived it — 111
// of them piled up and starved the Docker VM. Every user takes a fresh
// database inside it instead (`fresh_database`); ones older than a day are
// dropped on the way. `docker rm -f mcp-tools-test-pg` frees it.
//
// Shared by tests/support (integration tests) and the crate's own unit tests
// (pg.rs includes this file by path), so it names no crate items.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};

const CONTAINER: &str = "mcp-tools-test-pg";
const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

// The admin URL of the shared server, started (or restarted) if needed.
pub async fn shared_server() -> anyhow::Result<String> {
    let mut last = None;
    // Two test processes racing to create it: the loser's create fails on the
    // name, and its next attempt finds the winner's container.
    for _ in 0..5 {
        let started = Postgres::default()
            .with_name("pgvector/pgvector")
            .with_tag("pg16")
            .with_container_name(CONTAINER)
            // Test binaries run side by side (scripts/test.py), each with its
            // own servers and pools; the default 100 runs out.
            .with_cmd(["postgres", "-c", "max_connections=500"])
            .with_reuse(ReuseDirective::Always)
            .start()
            .await;
        match started {
            // Dropping a container marked for reuse leaves it running.
            Ok(container) => {
                let port = container.get_host_port_ipv4(5432).await?;
                let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
                wait_ready(&url).await?;
                return Ok(url);
            }
            Err(err) => last = Some(err),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow::anyhow!("test Postgres container: {}", last.map(|e| e.to_string()).unwrap_or_default()))
}

// A restarted container answers on its port before Postgres accepts.
async fn wait_ready(url: &str) -> anyhow::Result<()> {
    for _ in 0..120 {
        if let Ok((client, conn)) = tokio_postgres::connect(url, tokio_postgres::NoTls).await {
            tokio::spawn(conn);
            if client.simple_query("SELECT 1").await.is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("test Postgres at {url} never accepted a query")
}

// A new database's name: `t<unix seconds>_<random>`, so a sweep can tell its
// age. Its sibling `<name>_state` shares the prefix.
pub fn database_name() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    format!("t{secs}_{:08x}", rand::random::<u32>())
}

// Creates `name` on the server at `admin`, first dropping test databases
// left behind by runs more than a day old.
pub async fn fresh_database(admin: &str, name: &str) -> anyhow::Result<()> {
    let (client, conn) = tokio_postgres::connect(admin, tokio_postgres::NoTls).await?;
    tokio::spawn(conn);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    for row in client.query("SELECT datname FROM pg_database WHERE datname ~ '^t[0-9]+_'", &[]).await? {
        let old: String = row.get(0);
        let born = old[1..].split('_').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(now);
        if now.saturating_sub(born) > MAX_AGE.as_secs() {
            let _ = client.batch_execute(&format!("DROP DATABASE IF EXISTS {old} WITH (FORCE)")).await;
        }
    }
    // CREATE DATABASE copies template1 and fails while another process is
    // copying it too; parallel test binaries do that, so try again.
    let mut attempt = 0;
    loop {
        match client.batch_execute(&format!("CREATE DATABASE {name}")).await {
            Ok(()) => return Ok(()),
            Err(err) if attempt < 20 && format!("{err:?}").contains("is being accessed by other users") => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100 + 50 * attempt)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}
