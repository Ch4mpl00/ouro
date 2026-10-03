package mcp

import java.time.{Instant, ZoneId}

import zio.*
import zio.json.*
import zio.test.*
import zio.test.TestAspect.*

// Everything on the `mcp_state` database: the schema + seed, settings, the
// signal queue, the scheduler, the Telegram chat log.
object StateSpec extends ZIOSpecDefault:
  private def utc(s: String) = Instant.parse(s)

  private def exec(db: Db, sql: String) = db.pool.update(sql)

  // A scheduler over an empty task table (the seeded system tasks dropped).
  private def scheduler = TestPg.stateDb.tap(exec(_, "DELETE FROM scheduled_tasks")).map { db =>
    (ScheduledTasks(db, Settings(db)), Signals(db), db)
  }

  def spec = suite("state")(
    suite("db")(
      test("seeds the system tasks once and respects cancellations") {
        ZIO.scoped {
          for
            (url, schema) <- TestPg.freshSchema
            db <- Db.open(url, Some(schema))
            count = db.pool.query("SELECT count(*) AS n FROM scheduled_tasks")(_.getLong("n")).map(_.head)
            before <- count
            _ <- exec(db, "DELETE FROM scheduled_tasks WHERE source = 'dreaming'")
            // Opening it again (a restart) must not re-seed.
            _ <- Db.open(url, Some(schema))
            after <- count
          yield assertTrue(before == 3L, after == 2L)
        }
      },
      test("derives the state database from DATABASE_URL") {
        for url <- Db.stateUrl(PgUrl.parse("postgres://u:p@postgres:5432/mcp").toOption.get)
        yield assertTrue(url.database == "mcp_state", url.user.contains("u"), url.host == "postgres")
      },
      test("adopts a database the Rust server created") {
        // `state_migrations` present, Flyway history absent → baselined past
        // the seed instead of re-running it.
        ZIO.scoped {
          for
            pool <- TestPg.newsPool
            (url, schema) <- TestPg.freshSchema
            _ <- pool.update(s"CREATE SCHEMA $schema")
            _ <- pool.update(s"CREATE TABLE $schema.state_migrations (version integer PRIMARY KEY, applied_at timestamptz)")
            _ <- pool.update(s"INSERT INTO $schema.state_migrations VALUES (1, now())")
            db <- Db.open(url, Some(schema))
            history <- db.pool.query(s"SELECT version, type FROM $schema.flyway_schema_history")(rs =>
              rs.getString("version") -> rs.getString("type")
            )
          yield assertTrue(history == List("2" -> "BASELINE"))
        }
      },
    ),
    suite("settings")(
      test("rejects bogus names and follows the zone") {
        val at = utc("2026-10-01T22:30:00Z")
        assertTrue(
          Settings.parseTimezone("Mars/Olympus").isLeft,
          Settings.parseTimezone("+03:00").isLeft,
          Settings.localTime(Settings.parseTimezone("Europe/Kiev").toOption.get, at).display == "2026-10-02 01:30",
        )
      },
      test("defaults to UTC and round-trips") {
        ZIO.scoped {
          for
            db <- TestPg.stateDb
            s = Settings(db)
            before <- s.timezone
            _ <- s.setTimezone(ZoneId.of("Europe/Kiev"))
            after <- s.timezone
          yield assertTrue(before.getId == "UTC", after.getId == "Europe/Kiev")
        }
      },
    ),
    suite("signals")(
      test("pops in FIFO order exactly once, even concurrently") {
        ZIO.scoped {
          for
            db <- TestPg.stateDb
            s = Signals(db)
            a <- s.record("telegram", "hi")
            b <- s.record("scheduler", "tick")
            pending <- s.countPending
            both <- s.popNext.zipPar(s.popNext)
            empty <- s.popNext
            left <- s.countPending
          yield assertTrue(
            pending == 2L,
            List(both._1, both._2).flatten.map(_.id).sorted == List(a, b),
            empty.isEmpty,
            left == 0L,
          )
        }
      },
      test("lists by time and source without consuming") {
        ZIO.scoped {
          for
            db <- TestPg.stateDb
            s = Signals(db)
            _ <- s.record("telegram", "a") *> s.record("gmail", "b") *> s.record("telegram", "c")
            _ <- exec(db, "UPDATE signals SET created_at = '2026-10-01 10:00:00+00' WHERE content = 'a'")
            onlyTg <- s.list(ListSignals(source = Some("telegram")))
            // A real time comparison: same-day ISO no longer loses to the
            // sqlite text format the way string comparison did.
            after <- s.list(ListSignals(since = Time.parseJsDate("2026-10-01T10:00:00.000Z")))
            one <- s.list(ListSignals(limit = Some(1)))
            pending <- s.countPending
          yield assertTrue(
            onlyTg.map(_.content) == List("a", "c"),
            onlyTg.head.created_at == "2026-10-01 10:00:00",
            after.map(_.content) == List("b", "c"),
            one.size == 1,
            pending == 3L,
          )
        }
      },
      test("env context matches the TS wording") {
        val cfg = TelegramConfig(None, Some("123"), List("bills" -> 42L))
        assertTrue(
          Signals.envContext(TelegramConfig()).isEmpty,
          Signals.envContext(cfg).contains(
            "\n## Environment\nDefault Telegram chat id: 123.\nAvailable Telegram forum topics (name → thread_id):\n" +
              "  - bills: 42\nWhen sending a message that semantically belongs to one of these topics, pass the " +
              "matching messageThreadId to send_telegram_message."
          ),
        )
      },
    ),
    suite("scheduler")(
      test("evaluates cron on the user's wall clock") {
        val cron = Cron5.parse("0 9 * * *").toOption.get
        // 09:00 in Kyiv (UTC+3, summer time) is 06:00Z.
        assertTrue(Cron5.nextSlot(cron, ZoneId.of("Europe/Kiev"), utc("2026-07-01T00:00:00Z")).map(Time.iso).contains("2026-07-01T06:00:00.000Z"))
      },
      test("day-of-month and day-of-week are ORed like cron-parser") {
        // The 1st of the month OR any Monday. 2026-10-02 is a Friday, so the
        // next match is Monday the 5th, not November 1st.
        val cron = Cron5.parse("0 0 1 * 1").toOption.get
        assertTrue(Cron5.nextSlot(cron, ZoneId.of("UTC"), utc("2026-10-02T00:00:00Z")).map(Time.iso).contains("2026-10-05T00:00:00.000Z"))
      },
      test("fires a due slot once and retires one-shots") {
        ZIO.scoped {
          for
            (sched, signals, db) <- scheduler
            task <- sched.insert("30 14 2 10 *", false, "take pills", None)
            _ <- exec(db, s"UPDATE scheduled_tasks SET created_at = '2026-10-01 00:00:00+00' WHERE id = ${task.id}")
            early <- SchedulerPoller.tick(sched, signals, utc("2026-10-02T14:29:00Z"))
            due <- SchedulerPoller.tick(sched, signals, utc("2026-10-02T14:31:00Z"))
            again <- SchedulerPoller.tick(sched, signals, utc("2026-10-02T14:32:00Z"))
            active <- sched.listActive
            signal <- signals.popNext.someOrFailException
          yield assertTrue(
            TaskRow.of(task).recurring == 0,
            (early, due, again) == (0, 1, 0),
            active.isEmpty,
            signal.source == "scheduler",
            signal.content ==
              s"Scheduled task #${task.id} fired.\nCron: 30 14 2 10 *\nSlot: 2026-10-02T14:30:00.000Z\n" +
              "Now: 2026-10-02T14:31:00.000Z\nPrevious fire: never (this is the first run)\nRecurring: no (one-shot)\n\ntake pills",
          )
        }
      },
      test("a late tick advances by slot, not by wall clock") {
        ZIO.scoped {
          for
            (sched, signals, _) <- scheduler
            task <- sched.insert("0 * * * *", true, "hourly", Some("dreaming"))
            _ <- sched.markFired(task.id, utc("2026-10-02T10:00:00Z").getEpochSecond)
            // Three hours late: owes the 11:00 slot now, 12:00 on the next tick.
            fired <- SchedulerPoller.tick(sched, signals, utc("2026-10-02T13:05:00Z"))
            after <- sched.get(task.id).someOrFailException
            signal <- signals.popNext.someOrFailException
          yield assertTrue(
            fired == 1,
            after.lastRunAt.contains(utc("2026-10-02T11:00:00Z").getEpochSecond),
            signal.source == "dreaming",
            signal.content.contains("Previous fire: 2026-10-02T10:00:00.000Z"),
          )
        }
      },
    ),
    suite("telegram")(
      test("parses topics leniently in JSON order") {
        assertTrue(
          TelegramConfig.parseTopics(None).isEmpty,
          TelegramConfig.parseTopics(Some("[1,2]")).isEmpty,
          TelegramConfig.parseTopics(Some("not json")).isEmpty,
          TelegramConfig.parseTopics(Some("""{"news":7,"bank":"43","bills":42}""")) == List("news" -> 7L, "bills" -> 42L),
        )
      },
      test("signal content quotes the text and names the topic") {
        assertTrue(
          TelegramPoller.signalContent(5, Some(42), "hi \"there\"\nnext") ==
            "Telegram message in chat 5 (forum topic thread_id=42).\nText: \"hi \\\"there\\\"\\nnext\"",
          TelegramPoller.signalContent(5, None, "x") == "Telegram message in chat 5.\nText: \"x\"",
        )
      },
      test("ingest keeps only the default chat and advances the cursor") {
        val updates = """[
          { "update_id": 10, "message": { "message_id": 1, "chat": { "id": 7, "type": "private" }, "text": "hello" } },
          { "update_id": 11, "message": { "message_id": 2, "chat": { "id": 99, "type": "private" }, "text": "spam" } },
          { "update_id": 12, "edited_message": { "message_id": 3, "chat": { "id": 7, "type": "supergroup" }, "text": "edited", "message_thread_id": 4 } }
        ]""".fromJson[List[Update]].toOption.get
        ZIO.scoped {
          for
            db <- TestPg.stateDb
            log = ChatLog(db)
            signals = Signals(db)
            _ <- TelegramPoller.ingest(updates, 7, log, signals)
            cursor <- log.lastUpdateId
            history <- log.history(7, 50, None)
            topic <- log.history(7, 50, Some(4))
            pending <- signals.countPending
          yield assertTrue(
            cursor.contains(12L),
            history.map(_.text) == List("hello", "edited"),
            topic.size == 1,
            pending == 2L,
          )
        }
      },
      test("status frames cycle trailing dots") {
        assertTrue((0 until 5).map(StatusBubbles.render("Работаю", _)).toList == List("Работаю", "Работаю.", "Работаю..", "Работаю...", "Работаю"))
      },
    ),
  ) @@ withLiveClock @@ withLiveRandom
