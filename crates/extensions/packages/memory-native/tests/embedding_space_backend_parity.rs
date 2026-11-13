//! Embedding-space (fingerprint) vector search parity across the durable
//! filesystem backends.
//!
//! [`FilesystemMemoryDocumentRepository`] stores each chunk's vector under a
//! key derived from its embedder fingerprint and records the fingerprint
//! beside it; search ranks only vectors of the query's own space. Those keys
//! and the `Bytes` vector blob round-trip through each backend's indexed
//! projection (in-memory map, libSQL JSON, PostgreSQL JSONB), so the same
//! scenarios run on all three. The in-crate unit tests cover the in-memory
//! backend only.
//!
//! PostgreSQL leg: `DATABASE_URL` when set (one isolated database per test),
//! else a testcontainers server. Without either it skips with a notice on
//! stderr; `IRONCLAW_REQUIRE_POSTGRES` turns that skip into a failure.

use std::io::Write as _;
use std::sync::Arc;

use ironclaw_filesystem::{
    InMemoryBackend, IsolatedPostgresDatabase, IsolatedPostgresProvisioner, LibSqlRootFilesystem,
    PostgresRootFilesystem, PostgresUnreachable, RootFilesystem,
};
use ironclaw_memory::{MemoryDocumentPath, content_sha256};
use ironclaw_memory_native::{
    FilesystemMemoryDocumentRepository, MemoryChunkEmbedding, MemoryChunkWrite,
    MemoryDocumentIndexRepository, MemoryDocumentRepository, MemoryEmbeddingFingerprint,
    MemorySearchRequest,
};

type Repo = FilesystemMemoryDocumentRepository<dyn RootFilesystem>;

fn doc(relative: &str) -> MemoryDocumentPath {
    MemoryDocumentPath::new("tenant-a", "alice", Some("proj-1"), relative).unwrap()
}

fn embedding(fingerprint: &str, vector: Vec<f32>) -> MemoryChunkEmbedding {
    MemoryChunkEmbedding {
        fingerprint: MemoryEmbeddingFingerprint::new(fingerprint).unwrap(),
        vector,
    }
}

async fn write_embedded(
    repo: &Repo,
    path: &MemoryDocumentPath,
    content: &str,
    e: MemoryChunkEmbedding,
) {
    repo.write_document(path, content.as_bytes()).await.unwrap();
    repo.replace_document_chunks_if_current(
        path,
        &content_sha256(content),
        &[MemoryChunkWrite {
            content: content.to_string(),
            embedding: Some(e),
        }],
    )
    .await
    .unwrap();
}

/// Vector-only search: the query text matches no chunk, so every hit comes
/// from the vector branch.
async fn vector_hits(repo: &Repo, query: MemoryChunkEmbedding) -> Vec<String> {
    let scope = doc("x.md").scope().clone();
    repo.search_documents(
        &scope,
        &MemorySearchRequest::new("zzzqqq")
            .unwrap()
            .with_query_embedding(query)
            .with_min_vector_similarity(0.1),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|hit| hit.path.relative_path().to_string())
    .collect()
}

// -- Scenarios ----------------------------------------------------------------

/// Same dimension, identical vector, different model: only the query's own
/// space ranks, and within it cosine similarity orders the hits.
async fn only_the_query_space_ranks_in_cosine_order(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(fs);
    write_embedded(
        &repo,
        &doc("notes/other.md"),
        "other",
        embedding("model-b:2", vec![1.0, 0.0]),
    )
    .await;
    write_embedded(
        &repo,
        &doc("notes/near.md"),
        "near",
        embedding("model-a:2", vec![0.8, 0.6]),
    )
    .await;
    write_embedded(
        &repo,
        &doc("notes/exact.md"),
        "exact",
        embedding("model-a:2", vec![1.0, 0.0]),
    )
    .await;

    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/exact.md", "notes/near.md"],
        "only model-a vectors rank, best cosine first"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![1.0, 0.0])).await,
        vec!["notes/other.md"],
    );
    assert!(
        vector_hits(&repo, embedding("model-c:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "a space with no vectors ranks nothing"
    );
}

/// Re-indexing a document under a new embedder moves its vector to the new
/// space: the old space no longer finds it.
async fn reembedding_moves_a_document_between_spaces(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(fs);
    let path = doc("notes/moved.md");
    write_embedded(&repo, &path, "v1", embedding("model-a:2", vec![1.0, 0.0])).await;
    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/moved.md"]
    );

    write_embedded(&repo, &path, "v2", embedding("model-b:2", vec![0.0, 1.0])).await;
    assert!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "the old space's vector must not survive re-embedding"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![0.0, 1.0])).await,
        vec!["notes/moved.md"]
    );
}

// -- Backends -----------------------------------------------------------------

enum Guard {
    None,
    Dir(#[allow(dead_code, reason = "held only to keep the directory alive")] tempfile::TempDir),
    Postgres(Box<IsolatedPostgresDatabase>),
}

impl Guard {
    async fn finish(self) {
        if let Guard::Postgres(db) = self {
            db.cleanup().await;
        }
    }
}

type Fixture = Option<(Guard, Arc<dyn RootFilesystem>)>;

async fn in_memory() -> Fixture {
    Some((Guard::None, Arc::new(InMemoryBackend::new())))
}

async fn libsql() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let database = Arc::new(
        ::libsql::Builder::new_local(dir.path().join("memory.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = LibSqlRootFilesystem::new(database).unwrap();
    backend.run_migrations().await.unwrap();
    Some((Guard::Dir(dir), Arc::new(backend)))
}

const POSTGRES_URL_ENV: &str = "DATABASE_URL";

static POSTGRES: IsolatedPostgresProvisioner = IsolatedPostgresProvisioner::new(
    "postgres memory embedding-space parity",
    POSTGRES_URL_ENV,
    "memory_embedding_isolated_",
    PostgresUnreachable::Panic,
);

struct ContainerServer {
    config: tokio_postgres::Config,
    _container: testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::postgres::Postgres,
    >,
}

static CONTAINER: tokio::sync::OnceCell<Option<ContainerServer>> =
    tokio::sync::OnceCell::const_new();
static CONTAINER_DATABASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn skip_postgres(reason: &str) -> Fixture {
    assert!(
        std::env::var_os("IRONCLAW_REQUIRE_POSTGRES").is_none(),
        "IRONCLAW_REQUIRE_POSTGRES is set but the memory embedding PostgreSQL leg cannot run: \
         {reason}"
    );
    // Written past libtest's capture so a passing run that tested nothing on
    // PostgreSQL still says so. A failed write to stderr leaves nowhere else
    // to report the skip, so its result is deliberately discarded.
    #[allow(
        clippy::let_underscore_must_use,
        reason = "stderr write failure has no further reporting channel"
    )]
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED: postgres memory embedding-space parity did not run: {reason}"
    );
    None
}

async fn container_database() -> Result<tokio_postgres::Config, String> {
    let server = CONTAINER
        .get_or_init(|| async {
            use testcontainers_modules::testcontainers::{ImageExt, runners::AsyncRunner};
            let container = testcontainers_modules::postgres::Postgres::default()
                .with_user("postgres")
                .with_password("postgres")
                .with_tag("16-alpine")
                .start()
                .await
                .ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(5432).await.ok()?;
            let config = format!("postgres://postgres:postgres@{host}:{port}/postgres")
                .parse::<tokio_postgres::Config>()
                .ok()?;
            Some(ContainerServer {
                config,
                _container: container,
            })
        })
        .await
        .as_ref()
        .ok_or_else(|| {
            format!("{POSTGRES_URL_ENV} not set and docker/testcontainers unavailable")
        })?;
    let (admin, connection) = server
        .config
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|error| format!("container postgres unreachable ({error})"))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("container postgres admin connection ended: {error}");
        }
    });
    let name = format!(
        "memory_embedding_{}",
        CONTAINER_DATABASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    admin
        .execute(&format!("CREATE DATABASE {name}"), &[])
        .await
        .expect("create a per-test database in the container");
    let mut config = server.config.clone();
    config.dbname(&name);
    Ok(config)
}

async fn postgres() -> Fixture {
    let (guard, config) = if std::env::var_os(POSTGRES_URL_ENV).is_some() {
        let db = POSTGRES
            .provision()
            .await
            .expect("configured postgres provisions");
        let config = db.config().clone();
        (Guard::Postgres(Box::new(db)), config)
    } else {
        match container_database().await {
            Ok(config) => (Guard::None, config),
            Err(reason) => return skip_postgres(&reason),
        }
    };
    let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
    let pool = deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("postgres pool");
    let backend = PostgresRootFilesystem::new(pool);
    backend.run_migrations().await.expect("migrate postgres");
    Some((guard, Arc::new(backend)))
}

macro_rules! backend_suite {
    ($backend:ident: $($scenario:ident),+ $(,)?) => {
        mod $backend {
            $(
                #[tokio::test]
                async fn $scenario() {
                    let Some((guard, backend)) = super::$backend().await else {
                        return;
                    };
                    super::$scenario(backend).await;
                    guard.finish().await;
                }
            )+
        }
    };
}

backend_suite!(in_memory: only_the_query_space_ranks_in_cosine_order, reembedding_moves_a_document_between_spaces);
backend_suite!(libsql: only_the_query_space_ranks_in_cosine_order, reembedding_moves_a_document_between_spaces);
backend_suite!(postgres: only_the_query_space_ranks_in_cosine_order, reembedding_moves_a_document_between_spaces);
