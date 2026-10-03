package mcp.bin

// Discover chat ids: message the bot first, then run this.

import mcp.*
import zio.*

object TelegramGetChatId extends CliApp:
  def program(args: Chunk[String]) =
    for
      bot <- Cli.http.map(BotApi(_, TelegramConfig.fromEnv.botToken))
      updates <- bot.getUpdates(None, None)
      _ <-
        if updates.isEmpty then
          Console.printLine("No updates yet. Open Telegram, start a chat with your bot, send any message, then re-run.")
        else
          val chats = updates
            .flatMap(u => u.message.orElse(u.edited_message).orElse(u.channel_post).map(_.chat))
            .groupBy(_.id)
            .toList
            .sortBy(_._1)
            .map(_._2.head)
          Console.printLine(s"Found ${chats.size} distinct chat(s):\n") *>
            ZIO.foreachDiscard(chats)(c =>
              Console.printLine(s"  chat_id=${c.id}  type=${c.kind}  ${TelegramTools.chatLabel(c)}")
            ) *>
            Console.printLine("\nSet TELEGRAM_DEFAULT_CHAT_ID in .env to the chat_id you want notifications routed to.")
    yield ()
