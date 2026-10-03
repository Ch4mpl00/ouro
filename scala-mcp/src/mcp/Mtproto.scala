package mcp

// TEMPORARY: the MTProto client is written next.
import zio.*

final class MtprotoClient:
  def dialogs(limit: Int): Task[List[Dialog]] = Tools.fail("userbot: MTProto client not implemented yet")
  def channels(limit: Int): Task[List[ChannelHandle]] = Tools.fail("userbot: MTProto client not implemented yet")
  def history(channel: ChannelHandle, since: Option[Long], limit: Int): Task[List[ChannelMessage]] =
    Tools.fail("userbot: MTProto client not implemented yet")

object MtprotoClient:
  def connect(session: StringSession, creds: ApiCredentials): Task[MtprotoClient] = ZIO.succeed(MtprotoClient())
