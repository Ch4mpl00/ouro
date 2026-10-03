package mcp

// MTProto 2.0, the Telegram client protocol — what the userbot speaks. The
// JVM has no maintained MTProto library on Maven Central (TDLib is native and
// keeps its own session format), so this is a small client of our own:
//
// Sections:
//   1. schema    — TL definitions parsed from resources/tl/*.tl at startup
//   2. values    — `Tl`, a generic TL value (no generated classes)
//   3. codec     — schema-driven binary encode/decode
//   4. crypto    — AES-256-IGE, MTProto 2.0 message keys, RSA_PAD, pq factoring
//   5. transport — TCP with the "intermediate" framing
//   6. handshake — DH auth-key creation (a fresh, unauthorised key)
//   7. session   — encrypted messages: salts, msg ids, acks, rpc results
//   8. client    — initConnection + the three reads the userbot needs
//   9. login     — phone → code → 2FA (SRP), on a fresh key
//
// Reading the schema at runtime instead of generating ~1500 classes keeps
// this one file and makes a layer bump a resource swap. The userbot only
// reads a handful of fields, by name.

import zio.*

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.OutputStream
import java.math.BigInteger
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.MessageDigest
import java.security.SecureRandom
import java.time.Instant
import java.util.zip.GZIPInputStream
import javax.crypto.Cipher
import javax.crypto.spec.SecretKeySpec
import scala.collection.immutable.ListMap

// ── 1. schema ────────────────────────────────────────────────────────────────

final case class TlParam(name: String, tpe: String)

final case class TlCombinator(name: String, id: Int, params: List[TlParam], result: String, isFunction: Boolean)

final class TlSchema(val layer: Int, combinators: List[TlCombinator]):
  val byId: Map[Int, TlCombinator] = combinators.filterNot(_.isFunction).map(c => c.id -> c).toMap
  val byName: Map[String, TlCombinator] = combinators.map(c => c.name -> c).toMap

  def constructor(id: Int): Either[String, TlCombinator] = byId.get(id).toRight(f"unknown TL constructor 0x$id%08x")
  def named(name: String): Either[String, TlCombinator] = byName.get(name).toRight(s"unknown TL combinator $name")

object TlSchema:
  private val Line = """^([\w.]+)#([0-9a-f]{1,8})((?:\s+\{[^}]*\})*)(.*?)=\s*([^;]+);""".r
  private val Param = """([\w]+):([^\s]+)""".r

  def parse(sources: List[String]): TlSchema =
    val layer =
      sources.flatMap(_.linesIterator).collectFirst { case l if l.startsWith("// LAYER") => l.drop(8).trim.toInt }
    val combinators = sources.flatMap { src =>
      src.linesIterator
        .map(_.trim)
        .foldLeft((false, List.empty[TlCombinator])) { case ((functions, acc), line) =>
          if line == "---functions---" then (true, acc)
          else if line == "---types---" then (false, acc)
          else
            line match
              case Line(name, hex, _, params, result) =>
                val ps = Param.findAllMatchIn(params).map(m => TlParam(m.group(1), m.group(2))).toList
                (
                  functions,
                  TlCombinator(name, java.lang.Integer.parseUnsignedInt(hex, 16), ps, result.trim, functions) :: acc
                )
              case _ => (functions, acc)
        }
        ._2
        .reverse
    }
    TlSchema(layer.getOrElse(0), combinators)

  // api.tl + mtproto.tl from resources, parsed once.
  lazy val default: TlSchema =
    def read(name: String) = String(getClass.getResourceAsStream(s"/tl/$name").readAllBytes(), "UTF-8")
    parse(List(read("mtproto.tl"), read("api.tl")))

// ── 2. values ────────────────────────────────────────────────────────────────

// A decoded TL value. Constructors keep their schema name and fields by
// name; an optional field whose flag bit is clear is simply absent.
enum Tl:
  case Obj(constructor: String, fields: ListMap[String, Tl])
  case I(value: Int)
  case L(value: Long)
  case D(value: Double)
  case Str(value: String)
  case Bytes(value: Array[Byte])
  case Vec(items: Vector[Tl])
  case Bool(value: Boolean)
  // A `true`-typed flag that is set.
  case Flag

  def apply(field: String): Option[Tl] = this match
    case Obj(_, fs) => fs.get(field)
    case _          => None

  def name: String = this match
    case Obj(n, _) => n
    case other     => other.toString

  def int(field: String): Option[Int] = apply(field).collect { case I(v) => v }
  def long(field: String): Option[Long] = apply(field).collect { case L(v) => v; case I(v) => v.toLong }
  def str(field: String): Option[String] = apply(field).collect {
    case Str(v)   => v
    case Bytes(b) => String(b, java.nio.charset.StandardCharsets.UTF_8)
  }
  def vec(field: String): Vector[Tl] = apply(field).collect { case Vec(v) => v }.getOrElse(Vector.empty)
  def flag(field: String): Boolean = apply(field).exists { case Flag | Bool(true) => true; case _ => false }

object Tl:
  def obj(name: String, fields: (String, Tl)*): Obj = Obj(name, ListMap(fields*))

final class MtprotoError(val code: Int, val message: String) extends Exception(s"Telegram RPC error $code: $message")

// ── 3. codec ─────────────────────────────────────────────────────────────────

object TlCodec:
  private val VectorId = 0x1cb5c415
  val GzipPackedId = 0x3072cfa1
  val RpcResultId = 0xf35c6d01
  val MsgContainerId = 0x73f1f8dc

  final class Writer:
    private val out = ByteArrayOutputStream()
    private val buf = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN)

    def int(v: Int): Writer =
      buf.clear(); buf.putInt(v); out.write(buf.array, 0, 4); this
    def long(v: Long): Writer =
      buf.clear(); buf.putLong(v); out.write(buf.array, 0, 8); this
    def double(v: Double): Writer = long(java.lang.Double.doubleToRawLongBits(v))
    def raw(bytes: Array[Byte]): Writer =
      out.write(bytes); this

    // TL bytes: a 1- or 4-byte length prefix, padded to a multiple of 4.
    def bytes(b: Array[Byte]): Writer =
      val header =
        if b.length <= 253 then Array(b.length.toByte)
        else Array(254.toByte, b.length.toByte, (b.length >> 8).toByte, (b.length >> 16).toByte)
      out.write(header)
      out.write(b)
      val pad = (4 - (header.length + b.length) % 4) % 4
      out.write(Array.ofDim[Byte](pad))
      this

    def result: Array[Byte] = out.toByteArray

  final class Reader(data: Array[Byte]):
    private val buf = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN)
    def int: Int = buf.getInt
    def long: Long = buf.getLong
    def double: Double = buf.getDouble
    def raw(n: Int): Array[Byte] =
      val a = Array.ofDim[Byte](n); buf.get(a); a
    def remaining: Int = buf.remaining

    def bytes: Array[Byte] =
      val first = buf.get() & 0xff
      val (len, header) =
        if first <= 253 then (first, 1)
        else ((buf.get() & 0xff) | ((buf.get() & 0xff) << 8) | ((buf.get() & 0xff) << 16), 4)
      val b = raw(len)
      raw((4 - (header + len) % 4) % 4)
      b

  // ── encode ────────────────────────────────────────────────────────────────

  def encode(schema: TlSchema, value: Tl.Obj): Either[String, Array[Byte]] =
    scala.util.Try { val w = Writer(); writeBoxed(schema, w, value); w.result }.toEither.left.map(_.getMessage)

  private def fail(message: String): Nothing = throw IllegalArgumentException(message)

  private def writeBoxed(schema: TlSchema, w: Writer, value: Tl.Obj): Unit =
    val c = schema.named(value.name).fold(fail, identity)
    w.int(c.id)
    writeFields(schema, w, c, value)

  private def writeFields(schema: TlSchema, w: Writer, c: TlCombinator, value: Tl.Obj): Unit =
    c.params.foreach { p =>
      if p.tpe == "#" then
        // The flags word: one bit per optional param that is present.
        val bits = c.params.foldLeft(0) { (acc, q) =>
          flagRef(q.tpe) match
            case Some((field, bit, tpe)) if field == p.name && present(value(q.name), tpe) => acc | (1 << bit)
            case _                                                                         => acc
        }
        w.int(bits): Unit
      else
        flagRef(p.tpe) match
          case Some((_, _, tpe)) =>
            value(p.name)
              .filter(v => present(Some(v), tpe))
              .foreach(v => if tpe != "true" then writeValue(schema, w, tpe, v))
          case None =>
            writeValue(schema, w, p.tpe, value(p.name).getOrElse(fail(s"${c.name}: missing field ${p.name}")))
    }

  private def present(v: Option[Tl], tpe: String): Boolean = v match
    case None                                  => false
    case Some(Tl.Bool(false)) if tpe == "true" => false
    case Some(_)                               => true

  // "flags.3?Type" → (flags, 3, Type)
  private def flagRef(tpe: String): Option[(String, Int, String)] =
    val q = tpe.indexOf('?')
    if q < 0 then None
    else
      val (ref, inner) = (tpe.take(q), tpe.drop(q + 1))
      val dot = ref.indexOf('.')
      Some((ref.take(dot), ref.drop(dot + 1).toInt, inner))

  private def writeValue(schema: TlSchema, w: Writer, tpe: String, v: Tl): Unit = (tpe, v) match
    case ("int", Tl.I(x))                              => w.int(x): Unit
    case ("long", Tl.L(x))                             => w.long(x): Unit
    case ("long", Tl.I(x))                             => w.long(x.toLong): Unit
    case ("double", Tl.D(x))                           => w.double(x): Unit
    case ("string", Tl.Str(x))                         => w.bytes(x.getBytes("UTF-8")): Unit
    case ("string" | "bytes", Tl.Bytes(x))             => w.bytes(x): Unit
    case ("int128" | "int256", Tl.Bytes(x))            => w.raw(x): Unit
    case ("Bool", Tl.Bool(x))                          => w.int(if x then 0x997275b5 else 0xbc799737): Unit
    case (t, Tl.Vec(items)) if t.startsWith("Vector<") =>
      w.int(VectorId).int(items.size): Unit
      items.foreach(writeValue(schema, w, t.drop(7).dropRight(1), _))
    case (t, Tl.Vec(items)) if t.startsWith("vector<") =>
      w.int(items.size): Unit
      items.foreach(writeValue(schema, w, t.drop(7).dropRight(1), _))
    case (t, o: Tl.Obj) if t.headOption.exists(_.isLower) || t.startsWith("%") =>
      writeFields(schema, w, schema.named(o.name).fold(fail, identity), o)
    case (_, o: Tl.Obj) => writeBoxed(schema, w, o)
    case _              => fail(s"cannot write $v as $tpe")
  end writeValue

  // ── decode ────────────────────────────────────────────────────────────────

  def decode(schema: TlSchema, bytes: Array[Byte]): Either[String, Tl] = decodeAs(schema, bytes, "Object")

  def decodeAs(schema: TlSchema, bytes: Array[Byte], tpe: String): Either[String, Tl] =
    scala.util
      .Try(readValue(schema, Reader(bytes), tpe))
      .toEither
      .left
      .map(e => Option(e.getMessage).getOrElse(e.toString))

  private def readValue(schema: TlSchema, r: Reader, tpe: String): Tl = tpe match
    case "int"    => Tl.I(r.int)
    case "long"   => Tl.L(r.long)
    case "double" => Tl.D(r.double)
    // Kept as bytes: TL strings carry binary data too (pq, dh_prime, the
    // encrypted DH answer). `Tl.str` reads them as UTF-8 on demand.
    case "string"                     => Tl.Bytes(r.bytes)
    case "bytes"                      => Tl.Bytes(r.bytes)
    case "int128"                     => Tl.Bytes(r.raw(16))
    case "int256"                     => Tl.Bytes(r.raw(32))
    case t if t.startsWith("Vector<") =>
      val id = r.int
      if id != VectorId then fail(f"expected a vector, got 0x$id%08x")
      readItems(schema, r, t.drop(7).dropRight(1))
    case t if t.startsWith("vector<") => readItems(schema, r, t.drop(7).dropRight(1))
    case t if t.startsWith("%")       =>
      readBare(schema, r, schema.byName.values.find(c => c.result == t.drop(1) && !c.isFunction))
    case t if t.headOption.exists(_.isLower) => readBare(schema, r, schema.byName.get(t))
    case _                                   => readBoxed(schema, r)

  private def readItems(schema: TlSchema, r: Reader, inner: String): Tl =
    val n = r.int
    Tl.Vec(Vector.fill(n)(readValue(schema, r, inner)))

  private def readBare(schema: TlSchema, r: Reader, c: Option[TlCombinator]): Tl =
    readFields(schema, r, c.getOrElse(fail("unknown bare TL type")))

  private def readBoxed(schema: TlSchema, r: Reader): Tl =
    r.int match
      case 0x997275b5     => Tl.Bool(true)
      case 0xbc799737     => Tl.Bool(false)
      case `GzipPackedId` => readBoxed(schema, Reader(gunzip(r.bytes)))
      case `VectorId`     => fail("a vector where an object was expected (pass the element type)")
      case id             => readFields(schema, r, schema.constructor(id).fold(fail, identity))

  private def readFields(schema: TlSchema, r: Reader, c: TlCombinator): Tl =
    val flags = scala.collection.mutable.Map.empty[String, Int]
    val fields = c.params.flatMap { p =>
      if p.tpe == "#" then
        flags(p.name) = r.int
        None
      else
        flagRef(p.tpe) match
          case Some((field, bit, inner)) =>
            if (flags.getOrElse(field, 0) & (1 << bit)) == 0 then None
            else if inner == "true" then Some(p.name -> Tl.Flag)
            else Some(p.name -> readValue(schema, r, inner))
          case None => Some(p.name -> readValue(schema, r, p.tpe))
    }
    Tl.Obj(c.name, ListMap.from(fields))

  def gunzip(bytes: Array[Byte]): Array[Byte] = GZIPInputStream(ByteArrayInputStream(bytes)).readAllBytes()

// ── 4. crypto ────────────────────────────────────────────────────────────────

object MtCrypto:
  private val random = SecureRandom()

  def randomBytes(n: Int): Array[Byte] =
    val a = Array.ofDim[Byte](n); random.nextBytes(a); a

  def sha1(parts: Array[Byte]*): Array[Byte] = digest("SHA-1", parts)
  def sha256(parts: Array[Byte]*): Array[Byte] = digest("SHA-256", parts)

  private def digest(algorithm: String, parts: Seq[Array[Byte]]) =
    val md = MessageDigest.getInstance(algorithm)
    parts.foreach(md.update)
    md.digest()

  // AES in IGE mode: iv = (previous ciphertext block ‖ previous plaintext block).
  def igeEncrypt(data: Array[Byte], key: Array[Byte], iv: Array[Byte]): Array[Byte] = ige(data, key, iv, encrypt = true)
  def igeDecrypt(data: Array[Byte], key: Array[Byte], iv: Array[Byte]): Array[Byte] =
    ige(data, key, iv, encrypt = false)

  private def ige(data: Array[Byte], key: Array[Byte], iv: Array[Byte], encrypt: Boolean): Array[Byte] =
    require(data.length % 16 == 0, "IGE input must be a multiple of 16 bytes")
    val cipher = Cipher.getInstance("AES/ECB/NoPadding")
    cipher.init(if encrypt then Cipher.ENCRYPT_MODE else Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"))
    // For encryption x = plaintext, y = ciphertext; decryption swaps them.
    var prevY = iv.slice(0, 16)
    var prevX = iv.slice(16, 32)
    if !encrypt then
      val t = prevY; prevY = prevX; prevX = t
    val out = Array.ofDim[Byte](data.length)
    data.grouped(16).zipWithIndex.foreach { (block, i) =>
      val y = xor(cipher.doFinal(xor(block, prevY)), prevX)
      java.lang.System.arraycopy(y, 0, out, i * 16, 16)
      prevX = block
      prevY = y
    }
    out

  def xor(a: Array[Byte], b: Array[Byte]): Array[Byte] = Array.tabulate(a.length)(i => (a(i) ^ b(i)).toByte)

  // MTProto 2.0: x = 0 client → server, 8 server → client.
  def messageKey(authKey: Array[Byte], plaintext: Array[Byte], x: Int): Array[Byte] =
    sha256(authKey.slice(88 + x, 120 + x), plaintext).slice(8, 24)

  def aesKeyIv(authKey: Array[Byte], msgKey: Array[Byte], x: Int): (Array[Byte], Array[Byte]) =
    val a = sha256(msgKey, authKey.slice(x, x + 36))
    val b = sha256(authKey.slice(40 + x, 76 + x), msgKey)
    (a.slice(0, 8) ++ b.slice(8, 24) ++ a.slice(24, 32), b.slice(0, 8) ++ a.slice(8, 24) ++ b.slice(24, 32))

  // Unix time in the high 32 bits, the sub-second fraction in the low ones,
  // divisible by 4 (client message ids must be).
  def msgIdAt(epochMillis: Long): Long =
    val secs = epochMillis / 1000
    val fraction = ((epochMillis % 1000) << 32) / 1000
    ((secs << 32) | fraction) & ~3L

  def authKeyId(authKey: Array[Byte]): Long =
    ByteBuffer.wrap(sha1(authKey).slice(12, 20)).order(ByteOrder.LITTLE_ENDIAN).getLong

  final case class RsaKey(n: BigInteger, e: BigInteger):
    // Lower 64 bits of SHA1(TL string n ‖ TL string e).
    def fingerprint: Long =
      val w = TlCodec.Writer().bytes(unsigned(n)).bytes(unsigned(e)).result
      ByteBuffer.wrap(sha1(w).slice(12, 20)).order(ByteOrder.LITTLE_ENDIAN).getLong

  // Telegram's production RSA key (fingerprint d09d1d85de64fd85), verbatim
  // from tdesktop's mtproto_dc_options.cpp, parsed rather than transcribed.
  private val ProductionPem =
    """MIIBCgKCAQEA6LszBcC1LGzyr992NzE0ieY+BSaOW622Aa9Bd4ZHLl+TuFQ4lo4g
      |5nKaMBwK/BIb9xUfg0Q29/2mgIR6Zr9krM7HjuIcCzFvDtr+L0GQjae9H0pRB2OO
      |62cECs5HKhT5DZ98K33vmWiLowc621dQuwKWSQKjWf50XYFw42h21P2KXUGyp2y/
      |+aEyZ+uVgLLQbRA1dEjSDZ2iGRy12Mk5gpYc397aYp438fsJoHIgJ2lgMv5h7WY9
      |t6N/byY9Nw9p21Og3AoXSL2q/2IJ1WRUhebgAdGVMlV1fkuOQoEzR7EdpqtQD9Cs
      |5+bfo3Nhmcyvk5ftB0WkJ9z6bNZ7yxrP8wIDAQAB""".stripMargin

  val ProductionKey: RsaKey = parsePkcs1(ProductionPem)

  // PKCS#1 RSAPublicKey: SEQUENCE { INTEGER n, INTEGER e } in DER.
  def parsePkcs1(base64: String): RsaKey =
    val der = java.util.Base64.getMimeDecoder.decode(base64)
    var at = 0
    def length(): Int =
      val first = der(at) & 0xff
      at += 1
      if first < 0x80 then first
      else
        val n = first & 0x7f
        val len = (0 until n).foldLeft(0)((acc, i) => (acc << 8) | (der(at + i) & 0xff))
        at += n
        len
    def integer(): BigInteger =
      require(der(at) == 0x02, "expected a DER INTEGER")
      at += 1
      val len = length()
      val n = BigInteger(1, der.slice(at, at + len))
      at += len
      n
    require(der(at) == 0x30, "expected a DER SEQUENCE")
    at += 1
    val _ = length()
    val n = integer()
    RsaKey(n, integer())

  def unsigned(n: BigInteger): Array[Byte] =
    val b = n.toByteArray
    if b.length > 1 && b(0) == 0 then b.drop(1) else b

  def toFixed(n: BigInteger, size: Int): Array[Byte] =
    val b = unsigned(n)
    Array.ofDim[Byte](size - b.length) ++ b

  // RSA_PAD (MTProto 2.0): the key-exchange payload encrypted for the server.
  def rsaPad(data: Array[Byte], key: RsaKey): Array[Byte] =
    require(data.length <= 144, "RSA_PAD data must be at most 144 bytes")
    val padded = data ++ randomBytes(192 - data.length)
    Iterator
      .continually {
        val tempKey = randomBytes(32)
        val withHash = padded.reverse ++ sha256(tempKey, padded)
        val aesEncrypted = igeEncrypt(withHash, tempKey, Array.ofDim[Byte](32))
        val keyAesEncrypted = xor(tempKey, sha256(aesEncrypted)) ++ aesEncrypted
        BigInteger(1, keyAesEncrypted)
      }
      .find(_.compareTo(key.n) < 0)
      .map(m => toFixed(m.modPow(key.e, key.n), 256))
      .get

  // pq = p · q with p < q, by Pollard's rho (Brent): pq is a 63-bit number.
  def factor(pq: BigInteger): (BigInteger, BigInteger) =
    val one = BigInteger.ONE
    def rho(c: BigInteger): Option[BigInteger] =
      var x = BigInteger.TWO
      var y = x
      var d = one
      var steps = 0
      while d == one && steps < 1_000_000 do
        x = x.multiply(x).add(c).mod(pq)
        y = y.multiply(y).add(c).mod(pq)
        y = y.multiply(y).add(c).mod(pq)
        d = x.subtract(y).abs.gcd(pq)
        steps += 1
      Option.when(d != one && d != pq)(d)
    val p = LazyList.from(1).flatMap(c => rho(BigInteger.valueOf(c))).head
    val q = pq.divide(p)
    require(p.multiply(q) == pq, "pq factorisation failed")
    if p.compareTo(q) < 0 then (p, q) else (q, p)

// ── 5. transport ─────────────────────────────────────────────────────────────

// TCP with the "intermediate" framing: 0xeeeeeeee once, then each packet as a
// 4-byte little-endian length + payload. Blocking IO, run on ZIO's blocking
// pool; one request is in flight at a time (Session serialises).
final class MtConnection(socket: Socket):
  private val in = DataInputStream(socket.getInputStream)
  private val out: OutputStream = socket.getOutputStream

  def send(payload: Array[Byte]): Unit =
    out.write(ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(payload.length).array)
    out.write(payload)
    out.flush()

  def receive(): Array[Byte] =
    val header = Array.ofDim[Byte](4)
    in.readFully(header)
    val len = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
    val body = Array.ofDim[Byte](len)
    in.readFully(body)
    // A bare 4-byte packet is a transport error: -404 = unknown auth key.
    if len == 4 then throw MtprotoError(ByteBuffer.wrap(body).order(ByteOrder.LITTLE_ENDIAN).getInt, "transport error")
    body

  def close(): Unit = socket.close()

object MtConnection:
  def open(address: String, port: Int): ZIO[Scope, Throwable, MtConnection] =
    ZIO
      .fromAutoCloseable(ZIO.attemptBlocking {
        val socket = Socket()
        socket.connect(InetSocketAddress(address, port), 10_000)
        socket.setSoTimeout(30_000)
        socket.getOutputStream.write(Array.fill[Byte](4)(0xee.toByte))
        socket
      })
      .map(MtConnection(_))

// ── 6. handshake ─────────────────────────────────────────────────────────────

// A fresh auth key with the DC, by the documented DH exchange. The key is
// unauthorised until someone logs in with it; the server-side probe tests
// (help.getConfig) need nothing more.
object MtHandshake:
  final case class AuthResult(authKey: Array[Byte], salt: Long, timeOffset: Long)

  // Messages before a key exists: auth_key_id 0, msg id, length, body.
  private def plain(conn: MtConnection, schema: TlSchema, query: Tl.Obj): Tl =
    val body = TlCodec.encode(schema, query).fold(e => throw IllegalStateException(e), identity)
    val msgId = MtCrypto.msgIdAt(java.lang.System.currentTimeMillis())
    conn.send(TlCodec.Writer().long(0).long(msgId).int(body.length).raw(body).result)
    val r = TlCodec.Reader(conn.receive())
    val _ = (r.long, r.long) // auth_key_id 0, msg id
    val len = r.int
    TlCodec.decode(schema, r.raw(len)).fold(e => throw IllegalStateException(e), identity)

  private def bytesOf(t: Option[Tl]): Array[Byte] = t match
    case Some(Tl.Bytes(b)) => b
    case other             => throw IllegalStateException(s"expected bytes, got $other")

  def run(conn: MtConnection, schema: TlSchema, dc: Int): Task[AuthResult] = ZIO.attemptBlocking {
    import MtCrypto.*
    val nonce = randomBytes(16)
    val resPq = plain(conn, schema, Tl.obj("req_pq_multi", "nonce" -> Tl.Bytes(nonce)))
    val serverNonce = bytesOf(resPq("server_nonce"))
    val pq = BigInteger(1, bytesOf(resPq("pq")))
    val key = ProductionKey
    if !resPq.vec("server_public_key_fingerprints").contains(Tl.L(key.fingerprint)) then
      throw IllegalStateException("server offered no known RSA key")
    val (p, q) = factor(pq)
    val newNonce = randomBytes(32)
    val inner = TlCodec
      .encode(
        schema,
        Tl.obj(
          "p_q_inner_data_dc",
          "pq" -> Tl.Bytes(unsigned(pq)),
          "p" -> Tl.Bytes(unsigned(p)),
          "q" -> Tl.Bytes(unsigned(q)),
          "nonce" -> Tl.Bytes(nonce),
          "server_nonce" -> Tl.Bytes(serverNonce),
          "new_nonce" -> Tl.Bytes(newNonce),
          "dc" -> Tl.I(dc)
        )
      )
      .fold(e => throw IllegalStateException(e), identity)
      .drop(4) // bare: the constructor id is part of the RSA payload below
    val payload = TlCodec.Writer().int(schema.byName("p_q_inner_data_dc").id).raw(inner).result
    val dhParams = plain(
      conn,
      schema,
      Tl.obj(
        "req_DH_params",
        "nonce" -> Tl.Bytes(nonce),
        "server_nonce" -> Tl.Bytes(serverNonce),
        "p" -> Tl.Bytes(unsigned(p)),
        "q" -> Tl.Bytes(unsigned(q)),
        "public_key_fingerprint" -> Tl.L(key.fingerprint),
        "encrypted_data" -> Tl.Bytes(rsaPad(payload, key))
      )
    )
    if dhParams.name != "server_DH_params_ok" then throw IllegalStateException(s"DH params refused: ${dhParams.name}")

    val tmpKey = sha1(newNonce, serverNonce) ++ sha1(serverNonce, newNonce).slice(0, 12)
    val tmpIv = sha1(serverNonce, newNonce).slice(12, 20) ++ sha1(newNonce, newNonce) ++ newNonce.slice(0, 4)
    val answer = igeDecrypt(bytesOf(dhParams("encrypted_answer")), tmpKey, tmpIv)
    val innerDh = TlCodec.decode(schema, answer.drop(20)).fold(e => throw IllegalStateException(e), identity)
    val g = BigInteger.valueOf(innerDh.int("g").get.toLong)
    val dhPrime = BigInteger(1, bytesOf(innerDh("dh_prime")))
    val gA = BigInteger(1, bytesOf(innerDh("g_a")))
    val serverTime = innerDh.int("server_time").get.toLong
    if dhPrime.bitLength != 2048 || !dhPrime.isProbablePrime(64) then throw IllegalStateException("bad DH prime")
    val one = BigInteger.ONE
    if gA.compareTo(one) <= 0 || gA.compareTo(dhPrime.subtract(one)) >= 0 then throw IllegalStateException("bad g_a")

    val b = BigInteger(1, randomBytes(256))
    val gB = g.modPow(b, dhPrime)
    val authKey = toFixed(gA.modPow(b, dhPrime), 256)
    val clientInner = TlCodec
      .encode(
        schema,
        Tl.obj(
          "client_DH_inner_data",
          "nonce" -> Tl.Bytes(nonce),
          "server_nonce" -> Tl.Bytes(serverNonce),
          "retry_id" -> Tl.L(0),
          "g_b" -> Tl.Bytes(unsigned(gB))
        )
      )
      .fold(e => throw IllegalStateException(e), identity)
    val withHash = sha1(clientInner) ++ clientInner
    val padded = withHash ++ randomBytes((16 - withHash.length % 16) % 16)
    val result = plain(
      conn,
      schema,
      Tl.obj(
        "set_client_DH_params",
        "nonce" -> Tl.Bytes(nonce),
        "server_nonce" -> Tl.Bytes(serverNonce),
        "encrypted_data" -> Tl.Bytes(igeEncrypt(padded, tmpKey, tmpIv))
      )
    )
    if result.name != "dh_gen_ok" then throw IllegalStateException(s"DH key exchange failed: ${result.name}")
    val expected = sha1(newNonce, Array(1.toByte), sha1(authKey).slice(0, 8)).slice(4, 20)
    if !java.util.Arrays.equals(expected, bytesOf(result("new_nonce_hash1"))) then
      throw IllegalStateException("new_nonce_hash1 mismatch")
    val salt =
      ByteBuffer.wrap(xor(newNonce.slice(0, 8), serverNonce.slice(0, 8))).order(ByteOrder.LITTLE_ENDIAN).getLong
    AuthResult(authKey, salt, serverTime - java.lang.System.currentTimeMillis() / 1000)
  }

// ── 7. session ───────────────────────────────────────────────────────────────

// One encrypted session over one connection. Mutable by nature (salt, msg-id
// clock, sequence numbers, pending acks), so it lives behind a semaphore and
// is only touched on the blocking pool, one request at a time.
final class MtSession(
    conn: MtConnection,
    schema: TlSchema,
    authKey: Array[Byte],
    @volatile private var salt: Long,
    @volatile private var timeOffset: Long,
    lock: Semaphore
):
  import MtCrypto.*

  private val keyId = authKeyId(authKey)
  private val sessionId = ByteBuffer.wrap(randomBytes(8)).getLong
  private var lastMsgId = 0L
  private var contentSent = 0
  private var pendingAcks = Vector.empty[Long]

  private def nextMsgId(): Long =
    val id = msgIdAt(java.lang.System.currentTimeMillis() + timeOffset * 1000)
    lastMsgId = if id <= lastMsgId then lastMsgId + 4 else id
    lastMsgId

  private def nextSeq(content: Boolean): Int =
    if content then
      contentSent += 1; contentSent * 2 - 1
    else contentSent * 2

  private def sendEncrypted(body: Array[Byte], content: Boolean): Long =
    val msgId = nextMsgId()
    val header = TlCodec.Writer().long(salt).long(sessionId).long(msgId).int(nextSeq(content)).int(body.length).result
    val unpadded = header ++ body
    val padding = 12 + (16 - (unpadded.length + 12) % 16) % 16
    val plaintext = unpadded ++ randomBytes(padding)
    val msgKey = messageKey(authKey, plaintext, 0)
    val (key, iv) = aesKeyIv(authKey, msgKey, 0)
    conn.send(TlCodec.Writer().long(keyId).raw(msgKey).raw(igeEncrypt(plaintext, key, iv)).result)
    msgId

  // (msg id, seqno, body) of the next message from the server.
  private def receiveDecrypted(): (Long, Int, Array[Byte]) =
    val r = TlCodec.Reader(conn.receive())
    if r.long != keyId then throw IllegalStateException("message for another auth key")
    val msgKey = r.raw(16)
    val (key, iv) = aesKeyIv(authKey, msgKey, 8)
    val plaintext = igeDecrypt(r.raw(r.remaining), key, iv)
    if !java.util.Arrays.equals(messageKey(authKey, plaintext, 8), msgKey) then
      throw IllegalStateException("msg_key mismatch")
    val p = TlCodec.Reader(plaintext)
    val _ = p.long // salt
    if p.long != sessionId then throw IllegalStateException("message for another session")
    val msgId = p.long
    val seq = p.int
    val len = p.int
    (msgId, seq, p.raw(len))

  private def flushAcks(): Unit =
    if pendingAcks.nonEmpty then
      val ack = TlCodec.encode(schema, Tl.obj("msgs_ack", "msg_ids" -> Tl.Vec(pendingAcks.map(Tl.L(_))))).toOption.get
      pendingAcks = Vector.empty
      val _ = sendEncrypted(ack, content = false)

  private enum Outcome:
    case Result(value: Tl)
    case Resend
    case Continue

  // Service messages are handled here; the caller's rpc_result is returned.
  private def handle(msgId: Long, seq: Int, body: Array[Byte], waitingFor: Long, resultType: String): Outcome =
    if seq % 2 == 1 then pendingAcks :+= msgId
    val r = TlCodec.Reader(body)
    r.int match
      case TlCodec.MsgContainerId =>
        val n = r.int
        (0 until n).foldLeft(Outcome.Continue) { (acc, _) =>
          val innerId = r.long
          val innerSeq = r.int
          val len = r.int
          val outcome = handle(innerId, innerSeq, r.raw(len), waitingFor, resultType)
          if acc == Outcome.Continue then outcome else acc
        }
      case TlCodec.RpcResultId =>
        val req = r.long
        if req != waitingFor then Outcome.Continue
        else
          val rest = r.raw(r.remaining)
          val inner = TlCodec.Reader(rest)
          val id = inner.int
          val payload = if id == TlCodec.GzipPackedId then TlCodec.gunzip(inner.bytes) else rest
          val value = TlCodec.decodeAs(schema, payload, resultType).fold(e => throw IllegalStateException(e), identity)
          value match
            case o: Tl.Obj if o.name == "rpc_error" =>
              throw MtprotoError(o.int("error_code").getOrElse(0), o.str("error_message").getOrElse(""))
            case other => Outcome.Result(other)
      case GzipPacked if GzipPacked == TlCodec.GzipPackedId =>
        handle(msgId, 0, TlCodec.gunzip(r.bytes), waitingFor, resultType)
      case _ =>
        TlCodec.decode(schema, body).toOption match
          case Some(o: Tl.Obj) if o.name == "bad_server_salt" =>
            salt = o.long("new_server_salt").get
            Outcome.Resend
          // ping is answered by a bare pong, not an rpc_result.
          case Some(o: Tl.Obj) if o.name == "pong" && o.long("msg_id").contains(waitingFor) => Outcome.Result(o)
          case Some(o: Tl.Obj) if o.name == "new_session_created"                           =>
            salt = o.long("server_salt").get
            Outcome.Continue
          case Some(o: Tl.Obj) if o.name == "bad_msg_notification" =>
            o.int("error_code") match
              // msg_id too low / too high: resync the clock from the server's.
              case Some(16 | 17) =>
                timeOffset = (msgId >>> 32) - java.lang.System.currentTimeMillis() / 1000
                Outcome.Resend
              case code => throw MtprotoError(code.getOrElse(0), "bad_msg_notification")
          case _ => Outcome.Continue // acks, pongs, updates: not ours

  private val GzipPacked = TlCodec.GzipPackedId

  // Sends `query` (a function call) and waits for its result, resending on a
  // salt or clock correction and sleeping out a short FLOOD_WAIT.
  def invoke(query: Tl.Obj, resultType: String): Task[Tl] =
    val once = ZIO.attemptBlocking {
      val body = TlCodec.encode(schema, query).fold(e => throw IllegalArgumentException(e), identity)
      def attempt(resends: Int): Tl =
        flushAcks()
        val msgId = sendEncrypted(body, content = true)
        def loop(): Option[Tl] =
          val (id, seq, payload) = receiveDecrypted()
          handle(id, seq, payload, msgId, resultType) match
            case Outcome.Result(v) => Some(v)
            case Outcome.Resend    => None
            case Outcome.Continue  => loop()
        loop() match
          case Some(v)             => v
          case None if resends < 3 => attempt(resends + 1)
          case None                => throw IllegalStateException("server kept asking for a resend")
      attempt(0)
    }
    lock.withPermit(once).catchSome {
      case e: MtprotoError if e.message.startsWith("FLOOD_WAIT_") && e.message.drop(11).toIntOption.exists(_ <= 60) =>
        ZIO.logWarning(s"telegram: ${e.message}, waiting") *> ZIO
          .sleep(e.message.drop(11).toInt.seconds) *> invoke(query, resultType)
    }

object MtSession:
  def make(conn: MtConnection, schema: TlSchema, authKey: Array[Byte], salt: Long, timeOffset: Long): UIO[MtSession] =
    Semaphore.make(1).map(MtSession(conn, schema, authKey, salt, timeOffset, _))

// ── 8. client ────────────────────────────────────────────────────────────────

final class MtprotoClient(session: MtSession, schema: TlSchema):
  private def resultOf(method: String): String = schema.byName(method).result

  def call(query: Tl.Obj): Task[Tl] = session.invoke(query, resultOf(query.name))

  // Every dialog the account has, newest first, up to `limit`.
  private def allDialogs(limit: Int): Task[List[(Tl, Map[String, Tl])]] =
    def page(
        offsetDate: Int,
        offsetId: Int,
        offsetPeer: Tl,
        acc: List[(Tl, Map[String, Tl])]
    ): Task[List[(Tl, Map[String, Tl])]] =
      val want = (limit - acc.size).min(100)
      call(
        Tl.obj(
          "messages.getDialogs",
          "offset_date" -> Tl.I(offsetDate),
          "offset_id" -> Tl.I(offsetId),
          "offset_peer" -> offsetPeer,
          "limit" -> Tl.I(want),
          "hash" -> Tl.L(0)
        )
      ).flatMap { res =>
        val dialogs = res.vec("dialogs")
        val peers = MtprotoClient.peerIndex(res)
        val got = acc ++ dialogs.map(_ -> peers)
        val messages = res.vec("messages")
        val last = dialogs.lastOption
        val done = dialogs.isEmpty || res.name != "messages.dialogsSlice" || got.size >= limit
        if done then ZIO.succeed(got.take(limit))
        else
          val lastPeer = last.flatMap(_("peer")).get
          val top = last.flatMap(_.int("top_message")).getOrElse(0)
          val topMessage = messages.find(m => m.int("id").contains(top) && m("peer_id").contains(lastPeer))
          val inputPeer = MtprotoClient.inputPeer(lastPeer, peers).getOrElse(Tl.obj("inputPeerEmpty"))
          page(topMessage.flatMap(_.int("date")).getOrElse(0), top, inputPeer, got)
      }
    page(0, 0, Tl.obj("inputPeerEmpty"), Nil)

  def dialogs(limit: Int): Task[List[Dialog]] =
    allDialogs(limit).map(
      _.flatMap((d, peers) => d("peer").map(MtprotoClient.dialog(_, d.int("unread_count").getOrElse(0), peers)))
    )

  def channels(limit: Int): Task[List[ChannelHandle]] =
    allDialogs(limit).map(_.flatMap { (d, peers) =>
      d("peer").collect { case p if p.name == "peerChannel" => p.long("channel_id").get }.flatMap { id =>
        peers.get(s"channel:$id").map { ch =>
          ChannelHandle(id.toString, ch.str("title"), ch.str("username"), id, ch.long("access_hash").getOrElse(0L))
        }
      }
    })

  // Newest first, at most `limit`, only ids above `since` (exclusive).
  // Messages without text are skipped.
  def history(channel: ChannelHandle, since: Option[Long], limit: Int): Task[List[ChannelMessage]] =
    val peer =
      Tl.obj("inputPeerChannel", "channel_id" -> Tl.L(channel.channelId), "access_hash" -> Tl.L(channel.accessHash))
    def page(offsetId: Int, seen: Int, acc: List[ChannelMessage]): Task[List[ChannelMessage]] =
      val want = (limit - seen).min(100)
      call(
        Tl.obj(
          "messages.getHistory",
          "peer" -> peer,
          "offset_id" -> Tl.I(offsetId),
          "offset_date" -> Tl.I(0),
          "add_offset" -> Tl.I(0),
          "limit" -> Tl.I(want),
          "max_id" -> Tl.I(0),
          "min_id" -> Tl.I(since.fold(0)(_.toInt)),
          "hash" -> Tl.L(0)
        )
      ).flatMap { res =>
        val batch = res.vec("messages").filter(m => since.forall(s => m.int("id").exists(_ > s)))
        val texts = batch.flatMap(MtprotoClient.channelMessage)
        val total = seen + batch.size
        val lastId = batch.lastOption.flatMap(_.int("id"))
        if batch.isEmpty || total >= limit || lastId.isEmpty then ZIO.succeed(acc ++ texts)
        else page(lastId.get, total, acc ++ texts)
      }
    page(0, 0, Nil)

object MtprotoClient:
  // Connects to the session's home DC with its stored key and announces the
  // client (invokeWithLayer + initConnection) on a cheap first call.
  def connect(session: StringSession, creds: ApiCredentials): ZIO[Scope, Throwable, MtprotoClient] =
    for
      conn <- MtConnection.open(session.address, session.port)
      schema = TlSchema.default
      s <- MtSession.make(conn, schema, session.authKey, 0L, 0L)
      client = MtprotoClient(s, schema)
      _ <- s.invoke(wrapInit(schema, creds.apiId, Tl.obj("help.getConfig")), "Config")
    yield client

  // A fresh key on a fresh connection to `dc` — for login, before any
  // session exists.
  def anonymous(
      dc: Int,
      address: String,
      port: Int,
      apiId: Int
  ): ZIO[Scope, Throwable, (MtprotoClient, MtHandshake.AuthResult, Tl)] =
    for
      conn <- MtConnection.open(address, port)
      schema = TlSchema.default
      auth <- MtHandshake.run(conn, schema, dc)
      s <- MtSession.make(conn, schema, auth.authKey, auth.salt, auth.timeOffset)
      config <- s.invoke(wrapInit(schema, apiId, Tl.obj("help.getConfig")), "Config")
    yield (MtprotoClient(s, schema), auth, config)

  def wrapInit(schema: TlSchema, apiId: Int, query: Tl.Obj): Tl.Obj =
    Tl.obj(
      "invokeWithLayer",
      "layer" -> Tl.I(schema.layer),
      "query" -> Tl.obj(
        "initConnection",
        "api_id" -> Tl.I(apiId),
        "device_model" -> Tl.Str("mcp-tools"),
        "system_version" -> Tl.Str("JVM"),
        "app_version" -> Tl.Str(McpHandler.ServerVersion),
        "system_lang_code" -> Tl.Str("en"),
        "lang_pack" -> Tl.Str(""),
        "lang_code" -> Tl.Str("en"),
        "query" -> query
      )
    )

  // chats and users of a response, by "channel:<id>" / "chat:<id>" / "user:<id>".
  def peerIndex(res: Tl): Map[String, Tl] =
    val chats = res.vec("chats").flatMap { c =>
      c.long("id").map(id => (if c.name.startsWith("channel") then s"channel:$id" else s"chat:$id") -> c)
    }
    val users = res.vec("users").flatMap(u => u.long("id").map(id => s"user:$id" -> u))
    (chats ++ users).toMap

  def inputPeer(peer: Tl, peers: Map[String, Tl]): Option[Tl] = peer.name match
    case "peerChannel" =>
      val id = peer.long("channel_id").get
      peers
        .get(s"channel:$id")
        .map(c =>
          Tl.obj(
            "inputPeerChannel",
            "channel_id" -> Tl.L(id),
            "access_hash" -> Tl.L(c.long("access_hash").getOrElse(0L))
          )
        )
    case "peerChat" => peer.long("chat_id").map(id => Tl.obj("inputPeerChat", "chat_id" -> Tl.L(id)))
    case "peerUser" =>
      val id = peer.long("user_id").get
      peers
        .get(s"user:$id")
        .map(u =>
          Tl.obj("inputPeerUser", "user_id" -> Tl.L(id), "access_hash" -> Tl.L(u.long("access_hash").getOrElse(0L)))
        )
    case _ => None

  // gramjs's Dialog flags: a megagroup is a Channel there, so it counts as a
  // channel for both the dialog list and the harvester. Ids are Bot API
  // dialog ids (-100… for channels, -… for basic groups).
  def dialog(peer: Tl, unread: Int, peers: Map[String, Tl]): Dialog = peer.name match
    case "peerChannel" =>
      val id = peer.long("channel_id").get
      val ch = peers.get(s"channel:$id")
      Dialog(
        (-(1_000_000_000_000L + id)).toString,
        "channel",
        ch.flatMap(_.str("title")).getOrElse("(untitled)"),
        ch.flatMap(_.str("username")),
        unread
      )
    case "peerChat" =>
      val id = peer.long("chat_id").get
      Dialog(
        (-id).toString,
        "group",
        peers.get(s"chat:$id").flatMap(_.str("title")).getOrElse("(untitled)"),
        None,
        unread
      )
    case _ =>
      val id = peer.long("user_id").getOrElse(0L)
      val u = peers.get(s"user:$id")
      val name = List(u.flatMap(_.str("first_name")), u.flatMap(_.str("last_name"))).flatten.mkString(" ").trim
      Dialog(id.toString, "user", if name.isEmpty then "(untitled)" else name, u.flatMap(_.str("username")), unread)

  def channelMessage(m: Tl): Option[ChannelMessage] =
    if m.name != "message" then None
    else
      for
        id <- m.int("id")
        date <- m.int("date")
        text <- m.str("message").map(_.trim).filter(_.nonEmpty)
      yield ChannelMessage(
        id.toLong,
        Instant.ofEpochSecond(date.toLong),
        text,
        m.int("views").map(_.toLong),
        m.int("forwards").map(_.toLong)
      )

// ── 9. login ─────────────────────────────────────────────────────────────────

object MtLogin:
  final case class Prompts(phone: Task[String], code: Task[String], password: Option[String] => Task[String])

  final case class LoggedIn(session: StringSession, user: Tl)

  // The production DCs' addresses, for a PHONE_MIGRATE before any config.
  private val Dcs = Map(
    1 -> "149.154.175.53",
    2 -> "149.154.167.51",
    3 -> "149.154.175.100",
    4 -> "149.154.167.91",
    5 -> "91.108.56.130"
  )

  private def migrateTo(e: Throwable): Option[Int] = e match
    case m: MtprotoError if m.message.matches("(PHONE|NETWORK|USER)_MIGRATE_\\d+") =>
      m.message.split('_').last.toIntOption
    case _ => None

  // Phone → code → (2FA password) → the session for the DC that owns the
  // account. Not exercised against a live account by the tests: it needs a
  // phone and a login code.
  def run(creds: ApiCredentials, prompts: Prompts): ZIO[Scope, Throwable, LoggedIn] =
    def attempt(dc: Int, phone: String): ZIO[Scope, Throwable, LoggedIn] =
      val address = Dcs(dc)
      MtprotoClient.anonymous(dc, address, 443, creds.apiId).flatMap { (client, auth, _) =>
        client
          .call(
            Tl.obj(
              "auth.sendCode",
              "phone_number" -> Tl.Str(phone),
              "api_id" -> Tl.I(creds.apiId),
              "api_hash" -> Tl.Str(creds.apiHash),
              "settings" -> Tl.obj("codeSettings")
            )
          )
          .foldZIO(
            e => migrateTo(e).fold(ZIO.fail(e))(attempt(_, phone)),
            sent =>
              for
                code <- prompts.code
                hash = sent.str("phone_code_hash").getOrElse("")
                signedIn <- client
                  .call(
                    Tl.obj(
                      "auth.signIn",
                      "phone_number" -> Tl.Str(phone),
                      "phone_code_hash" -> Tl.Str(hash),
                      "phone_code" -> Tl.Str(code)
                    )
                  )
                  .catchSome {
                    case e: MtprotoError if e.message == "SESSION_PASSWORD_NEEDED" => checkPassword(client, prompts)
                  }
                user <- ZIO
                  .fromOption(signedIn("user"))
                  .orElseFail(RuntimeException(s"unexpected sign-in result ${signedIn.name}"))
              yield LoggedIn(StringSession(dc, address, 443, auth.authKey), user)
          )
      }
    prompts.phone.flatMap(phone => attempt(2, phone.trim))

  private def checkPassword(client: MtprotoClient, prompts: Prompts): Task[Tl] =
    for
      pwd <- client.call(Tl.obj("account.getPassword"))
      password <- prompts.password(pwd.str("hint"))
      srp <- ZIO.attempt(Srp.check(pwd, password.trim))
      done <- client.call(Tl.obj("auth.checkPassword", "password" -> srp))
    yield done

// The 2FA proof (inputCheckPasswordSRP) for account.password with the
// SHA256-SHA256-PBKDF2-HMAC-SHA512-iter100000-SHA256-ModPow algorithm.
object Srp:
  import MtCrypto.*

  private def bytes(t: Option[Tl]): Array[Byte] = t.collect { case Tl.Bytes(b) => b }.getOrElse(Array.emptyByteArray)
  private def sh(data: Array[Byte], salt: Array[Byte]) = sha256(salt, data, salt)
  private def pad(n: BigInteger) = toFixed(n, 256)

  // PBKDF2-HMAC-SHA512, one 64-byte block, over raw bytes. The JDK's
  // PBEKeySpec takes chars and would re-encode this binary "password".
  def pbkdf2Sha512(password: Array[Byte], salt: Array[Byte], iterations: Int): Array[Byte] =
    val mac = javax.crypto.Mac.getInstance("HmacSHA512")
    mac.init(SecretKeySpec(password, "HmacSHA512"))
    val first = mac.doFinal(salt ++ Array[Byte](0, 0, 0, 1))
    Iterator.iterate(first)(mac.doFinal).take(iterations).reduce(xor)

  def check(password: Tl, plain: String): Tl.Obj =
    val algo = password("current_algo").getOrElse(throw IllegalStateException("account has no 2FA algorithm"))
    val (salt1, salt2) = (bytes(algo("salt1")), bytes(algo("salt2")))
    val g = BigInteger.valueOf(algo.int("g").get.toLong)
    val p = BigInteger(1, bytes(algo("p")))
    val gB = BigInteger(1, bytes(password("srp_B")))
    val srpId = password.long("srp_id").get

    val ph1 = sh(sh(plain.getBytes("UTF-8"), salt1), salt2)
    val x = BigInteger(1, sh(pbkdf2Sha512(ph1, salt1, 100000), salt2))
    val v = g.modPow(x, p)
    val k = BigInteger(1, sha256(pad(p), pad(g)))
    val a = BigInteger(1, randomBytes(256))
    val gA = g.modPow(a, p)
    val u = BigInteger(1, sha256(pad(gA), pad(gB)))
    val t = gB.subtract(k.multiply(v).mod(p)).mod(p)
    val sA = t.modPow(a.add(u.multiply(x)), p)
    val kA = sha256(pad(sA))
    val m1 = sha256(xor(sha256(pad(p)), sha256(pad(g))), sha256(salt1), sha256(salt2), pad(gA), pad(gB), kA)
    Tl.obj("inputCheckPasswordSRP", "srp_id" -> Tl.L(srpId), "A" -> Tl.Bytes(pad(gA)), "M1" -> Tl.Bytes(m1))
