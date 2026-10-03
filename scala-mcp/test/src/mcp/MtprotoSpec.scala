package mcp

import java.math.BigInteger
import java.util.HexFormat

import zio.*
import zio.test.*
import zio.test.TestAspect.*

object MtprotoSpec extends ZIOSpecDefault:
  private val hex = HexFormat.of()
  private def h(s: String) = hex.parseHex(s.toLowerCase)
  private val schema = TlSchema.default

  def spec = suite("mtproto")(
    test("parses the schema and knows the layer") {
      assertTrue(
        schema.layer == 229,
        schema.byName("messages.getHistory").isFunction,
        schema.byName("messages.getHistory").id == 0x4423e6c5,
        schema.constructor(0x7600b9d3).map(_.name) == Right("message"),
      )
    },
    test("AES-IGE matches the OpenSSL test vectors") {
      val key1 = h("000102030405060708090A0B0C0D0E0F")
      val iv1 = h("000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F")
      val out1 = h("1A8519A6557BE652E9DA8E43DA4EF4453CF456B4CA488AA383C79C98B34797CB")
      val key2 = "This is an imple".getBytes("ASCII")
      val iv2 = "mentation of IGE mode for OpenSS".getBytes("ASCII")
      // OpenSSL's second vector encrypts to readable ASCII.
      val in2 = h("4C2E204C6574277320686F70652042656E20676F74206974207269676874210A")
      val out2 = h("99706487A1CDE613BC6DE0B6F24B1C7AA448C8B9C3403E3467A8CAD89340F53B")
      assertTrue(
        MtCrypto.igeEncrypt(Array.ofDim(32), key1, iv1).sameElements(out1),
        MtCrypto.igeDecrypt(out1, key1, iv1).sameElements(Array.ofDim[Byte](32)),
        MtCrypto.igeEncrypt(out2, key2, iv2).sameElements(in2),
        MtCrypto.igeDecrypt(in2, key2, iv2).sameElements(out2),
      )
    },
    test("factors pq like the documented example") {
      val (p, q) = MtCrypto.factor(BigInteger("17ED48941A08F981", 16))
      assertTrue(p == BigInteger("494C553B", 16), q == BigInteger("53911073", 16))
    },
    test("message ids carry the unix time and are divisible by 4") {
      val id = MtCrypto.msgIdAt(1_790_000_000_123L)
      assertTrue(id >>> 32 == 1_790_000_000L, id % 4 == 0, MtCrypto.msgIdAt(1_790_000_000_124L) > id)
    },
    test("PBKDF2-HMAC-SHA512 matches the RFC test vector") {
      // RFC 6070-style vector for SHA-512 (password "password", salt "salt", 1 iteration).
      val out = Srp.pbkdf2Sha512("password".getBytes, "salt".getBytes, 1)
      assertTrue(
        hex.formatHex(out) ==
          "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce"
      )
    },
    test("the production key has its known fingerprint") {
      assertTrue(MtCrypto.ProductionKey.fingerprint == 0xd09d1d85de64fd85L)
    },
    test("encodes a request byte for byte") {
      val nonce = Array.tabulate[Byte](16)(_.toByte)
      val bytes = TlCodec.encode(schema, Tl.obj("req_pq_multi", "nonce" -> Tl.Bytes(nonce))).toOption.get
      assertTrue(hex.formatHex(bytes) == "f18e7ebe" + hex.formatHex(nonce))
    },
    test("round-trips a flagged constructor, strings and vectors") {
      val message = Tl.obj(
        "message",
          "post" -> Tl.Flag,
          "id" -> Tl.I(7),
          "peer_id" -> Tl.obj("peerChannel", "channel_id" -> Tl.L(100L)),
          "date" -> Tl.I(1_780_000_000),
          "message" -> Tl.Str("привет " * 100),
          "views" -> Tl.I(123),
          "forwards" -> Tl.I(4),
          "restriction_reason" -> Tl.Vec(Vector.empty),
      )
      val decoded = TlCodec.encode(schema, message).flatMap(TlCodec.decode(schema, _))
      val m = decoded.toOption.get
      assertTrue(
        m.flag("post"),
        !m.flag("out"),
        m.int("views").contains(123),
        m("peer_id").flatMap(_.long("channel_id")).contains(100L),
        MtprotoClient.channelMessage(m).map(_.text) == Some(("привет " * 100).trim),
        m("media").isEmpty,
      )
    },
    test("an encrypted message decrypts with the server-side key schedule") {
      val authKey = MtCrypto.randomBytes(256)
      val plaintext = MtCrypto.randomBytes(64)
      val msgKey = MtCrypto.messageKey(authKey, plaintext, 0)
      val (key, iv) = MtCrypto.aesKeyIv(authKey, msgKey, 0)
      val back = MtCrypto.igeDecrypt(MtCrypto.igeEncrypt(plaintext, key, iv), key, iv)
      assertTrue(back.sameElements(plaintext), MtCrypto.messageKey(authKey, back, 0).sameElements(msgKey))
    },
    test("maps dialogs to Bot API ids and kinds") {
      val peers = Map(
        "channel:5" -> Tl.obj("channel", "id" -> Tl.L(5), "title" -> Tl.Str("News"), "username" -> Tl.Str("news"), "access_hash" -> Tl.L(9)),
        "chat:6" -> Tl.obj("chat", "id" -> Tl.L(6), "title" -> Tl.Str("Family")),
        "user:7" -> Tl.obj("user", "id" -> Tl.L(7), "first_name" -> Tl.Str("Ann"), "last_name" -> Tl.Str("Lee")),
      )
      val ch = MtprotoClient.dialog(Tl.obj("peerChannel", "channel_id" -> Tl.L(5)), 3, peers)
      val chat = MtprotoClient.dialog(Tl.obj("peerChat", "chat_id" -> Tl.L(6)), 0, peers)
      val user = MtprotoClient.dialog(Tl.obj("peerUser", "user_id" -> Tl.L(7)), 1, peers)
      assertTrue(
        ch == Dialog("-1000000000005", "channel", "News", Some("news"), 3),
        chat == Dialog("-6", "group", "Family", None, 0),
        user == Dialog("7", "user", "Ann Lee", None, 1),
      )
    },
    // Against Telegram itself: a fresh anonymous auth key with DC 2 and a ping
    // over the encrypted session. Touches no account. Opt-in (network):
    //   MTPROTO_LIVE=1 ./mill test.testOnly mcp.MtprotoSpec
    test("handshakes with a real DC and round-trips an encrypted ping") {
      ZIO.scoped {
        for
          conn <- MtConnection.open("149.154.167.51", 443)
          auth <- MtHandshake.run(conn, schema, 2)
          session <- MtSession.make(conn, schema, auth.authKey, auth.salt, auth.timeOffset)
          pong <- session.invoke(Tl.obj("ping", "ping_id" -> Tl.L(42)), "Pong")
          nearest <- session.invoke(Tl.obj("help.getNearestDc"), "NearestDc").either
          // A nested invokeWithLayer(initConnection(help.getConfig)): the
          // server answers with the whole Config, decoded by schema alone.
          init <- session.invoke(MtprotoClient.wrapInit(schema, 1, Tl.obj("help.getConfig")), "Config").either
        yield assertTrue(
          auth.authKey.length == 256,
          pong.long("ping_id").contains(42L),
          // Either an answer or a refusal — both arrive as an encrypted
          // rpc_result we decrypted and decoded.
          nearest.fold({ case e: MtprotoError => e.code > 0; case _ => false }, _.name == "nearestDc"),
          init.fold(
            { case e: MtprotoError => e.message == "API_ID_INVALID"; case _ => false },
            c => c.name == "config" && c.int("this_dc").contains(2) && c.vec("dc_options").nonEmpty,
          ),
        )
      }
    } @@ ifEnvSet("MTPROTO_LIVE") @@ timeout(60.seconds),
  ) @@ withLiveClock
