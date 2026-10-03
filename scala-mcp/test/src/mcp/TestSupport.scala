package mcp

// Test doubles and the throwaway Postgres every database-backed spec shares.
//
// Postgres: TEST_DATABASE_URL when set (a throwaway pgvector database, never
// prod), otherwise one Testcontainers pgvector container for the whole test
// JVM. The news schema is migrated once; each state-database test gets a
// fresh schema, so parallel specs never see each other's rows.

import java.util.concurrent.atomic.AtomicBoolean

import com.dimafeng.testcontainers.PostgreSQLContainer
import org.testcontainers.utility.DockerImageName
import zio.*

object TestPg:
  private lazy val url: PgUrl =
    Env.get("TEST_DATABASE_URL").flatMap(PgUrl.parse(_).toOption).getOrElse {
      val container = PostgreSQLContainer(
        dockerImageNameOverride = DockerImageName.parse("pgvector/pgvector:pg16").asCompatibleSubstituteFor("postgres"),
        databaseName = "mcp",
        username = "mcp",
        password = "mcp",
      )
      container.start()
      PgUrl(container.host, container.mappedPort(5432), "mcp", Some("mcp"), Some("mcp"))
    }

  private val migrated = Unsafe.unsafe(implicit u => Semaphore.unsafe.make(1))
  @volatile private var newsReady = false

  // The news / memory store, migrated (once per JVM).
  def newsPool: ZIO[Scope, Throwable, PgPool] =
    for
      u <- ZIO.attemptBlocking(url)
      pool <- PgPool.make(u, maxSize = 4)
      _ <- migrated.withPermit(ZIO.unless(newsReady)(Migrations.migrateNews(pool) *> ZIO.succeed { newsReady = true }))
    yield pool

  def baseUrl: Task[PgUrl] = ZIO.attemptBlocking(url)

  // A fresh schema name and the URL that selects it.
  def freshSchema: Task[(PgUrl, String)] =
    for
      u <- ZIO.attemptBlocking(url)
      n <- Random.nextLong.map(java.lang.Long.toHexString)
      schema = s"t_$n"
    yield (u.copy(options = Map("currentSchema" -> schema)), schema)

  // A fresh `mcp_state` schema, migrated and seeded like a new database.
  def stateDb: ZIO[Scope, Throwable, Db] = freshSchema.flatMap((u, s) => Db.open(u, Some(s)))

// Deterministic stand-in for text-embedding-3-small: a bag-of-words vector,
// so distance tracks word overlap. Normalised like the real one.
final class FakeEmbedder(dims: Int) extends Embedder:
  val down = AtomicBoolean(false)

  def embedBatch(texts: List[String]): Task[List[Vector[Float]]] =
    if down.get then ZIO.fail(RuntimeException("provider unreachable"))
    else ZIO.succeed(texts.map(FakeEmbedder.embed(_, dims)))

object FakeEmbedder:
  def apply(dims: Int = 64): FakeEmbedder = new FakeEmbedder(dims)

  def embed(text: String, dims: Int): Vector[Float] =
    val v = Array.fill(dims)(0f)
    text.toLowerCase.split("[^\\p{L}\\p{N}]+").filter(_.nonEmpty).foreach { token =>
      val hash = token.codePoints.toArray.foldLeft(0L)((h, c) => (h * 31 + c) % dims)
      v(hash.toInt) += 1
    }
    val norm = math.sqrt(v.map(x => x * x).sum).toFloat
    v.map(x => if norm > 0 then x / norm else x).toVector

// The memory store as plain data, literal enough that passing here means the
// same thing against Postgres.
final class InMemoryStore(state: Ref[InMemoryStore.State]) extends MemoryStore:
  import InMemoryStore.*

  // Fires just before an updateDoc CAS: simulates another agent committing
  // between this one's read and write.
  @volatile var beforeUpdate: Option[Long => UIO[Unit]] = None

  private def next: UIO[Long] = state.modify(s => (s.seq + 1, s.copy(seq = s.seq + 1)))

  def createProject(slug: String, title: String): Task[Project] =
    for
      id <- next
      now <- Clock.instant
      p = Project(id, slug, title, now, now)
      _ <- state.update(s => s.copy(projects = s.projects :+ p))
    yield p

  def getProject(slug: String): Task[Option[Project]] = state.get.map(_.projects.find(_.slug == slug))
  def listProjects: Task[List[Project]] = state.get.map(_.projects.sortBy(_.slug))

  def listDocs(projectId: Long): Task[List[DocSummary]] =
    state.get.map(_.docs.filter(_.projectId == projectId).map(_.summaryView).sortBy(_.name))

  def getDoc(projectId: Long, name: String): Task[Option[Doc]] =
    state.get.map(_.docs.find(d => d.projectId == projectId && d.name == name))

  def createDoc(projectId: Long, name: String, summary: Option[String], body: String): Task[Doc] =
    for
      id <- next
      now <- Clock.instant
      d = Doc(id, projectId, name, summary, body, 1, bytes(body), now)
      _ <- state.update(s => s.copy(docs = s.docs :+ d))
    yield d

  def updateDoc(docId: Long, expectedVersion: Int, body: String, summary: Option[Option[String]]): Task[Option[Doc]] =
    ZIO.foreachDiscard(beforeUpdate)(_(docId)) *> Clock.instant.flatMap { now =>
      state.modify { s =>
        s.docs.find(_.id == docId).filter(_.version == expectedVersion) match
          case None => (None, s)
          case Some(d) =>
            val updated = d.copy(
              body = body,
              summary = summary.getOrElse(d.summary),
              version = d.version + 1,
              sizeBytes = bytes(body),
              updatedAt = now,
            )
            (Some(updated), s.copy(docs = s.docs.map(x => if x.id == docId then updated else x)))
      }
    }

  def insertPatch(p: NewPatch): Task[DocPatch] =
    Clock.instant.flatMap { now =>
      val patch = DocPatch(p.pid, p.docId, p.kind, p.edits, p.bodyBefore, p.versionBefore, p.versionAfter, p.actor, p.rationale, now)
      state.update(s => s.copy(patches = s.patches :+ patch)).as(patch)
    }

  def listPatches(docId: Long, limit: Int): Task[List[DocPatch]] =
    state.get.map(_.patches.reverse.filter(_.docId == docId).take(limit))

  def getPatch(docId: Long, pid: String): Task[Option[DocPatch]] =
    state.get.map(_.patches.find(p => p.docId == docId && p.pid == pid))

  def createFact(body: String, tags: List[String], source: Option[String]): Task[Fact] =
    for
      id <- next
      now <- Clock.instant
      f = Fact(id, body, tags, source, MemoryState.active, now, now)
      _ <- state.update(s => s.copy(facts = s.facts :+ f))
    yield f

  def getFact(id: Long): Task[Option[Fact]] = state.get.map(_.facts.find(_.id == id))
  def getFactBySource(source: String): Task[Option[Fact]] = state.get.map(_.facts.find(_.source.contains(source)))

  def updateFact(id: Long, update: FactUpdate): Task[Option[Fact]] =
    Clock.instant.flatMap { now =>
      state.modify { s =>
        s.facts.find(_.id == id) match
          case None => (None, s)
          case Some(f) =>
            val u = f.copy(
              body = update.body.getOrElse(f.body),
              tags = update.tags.getOrElse(f.tags),
              state = update.state.getOrElse(f.state),
              updatedAt = now,
            )
            (Some(u), s.copy(facts = s.facts.map(x => if x.id == id then u else x)))
      }
    }

  def replaceIndex(sourceRef: String, entries: List[IndexUpsert]): Task[Unit] =
    ZIO.foreach(entries)(e => next.map(_ -> e)).flatMap { numbered =>
      state.update(s => s.copy(index = s.index.filterNot(_._2.sourceRef == sourceRef) ++ numbered))
    }

  def searchIndex(embedding: Vector[Float], limit: Int, states: List[MemoryState], tags: List[String])
      : Task[List[IndexHit]] =
    val wanted = tags.filter(_.nonEmpty)
    state.get.map(
      _.index
        .collect {
          case (id, e) if e.embedding.isDefined && states.contains(e.state) && (wanted.isEmpty || e.tags.exists(wanted.contains)) =>
            IndexHit(id, e.ref, e.text, e.tags, e.actor, e.state, e.ts, cosine(e.embedding.get, embedding))
        }
        .sortBy(_.distance)
        .take(limit)
    )

  def listUnembedded(limit: Int): Task[List[(Long, String)]] =
    state.get.map(_.index.filter(_._2.embedding.isEmpty).take(limit).map((id, e) => id -> e.text))

  def setEmbedding(id: Long, embedding: Vector[Float]): Task[Unit] =
    state.update(s => s.copy(index = s.index.map((i, e) => if i == id then i -> e.copy(embedding = Some(embedding)) else i -> e)))

object InMemoryStore:
  final case class State(
      seq: Long = 0,
      projects: List[Project] = Nil,
      docs: List[Doc] = Nil,
      patches: List[DocPatch] = Nil,
      facts: List[Fact] = Nil,
      index: List[(Long, IndexUpsert)] = Nil,
  )

  def make: UIO[InMemoryStore] = Ref.make(State()).map(InMemoryStore(_))

  private def bytes(s: String) = s.getBytes(java.nio.charset.StandardCharsets.UTF_8).length

  private def cosine(a: Vector[Float], b: Vector[Float]): Double =
    val dot = a.zip(b).map((x, y) => x.toDouble * y).sum
    val na = math.sqrt(a.map(x => x.toDouble * x).sum)
    val nb = math.sqrt(b.map(x => x.toDouble * x).sum)
    if na == 0 || nb == 0 then 1.0 else 1.0 - dot / (na * nb)
