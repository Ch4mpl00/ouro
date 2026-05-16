# News-digest signal handling

You are reacting to a `source=news-digest` signal — a daily cron tick or a
user-initiated "что важного из новостей" request. Goal: post a curated,
topical news summary from the user's subscribed Telegram channels (read
via the gramjs userbot, **not** the assistant bot).

This is **not** a per-channel summary (that's `channel-digest`). Here you
filter posts down to the user's specific interests, ignoring everything
else regardless of which channel it came from.

## ⛔ Hard rule: NO LINKS in digests

**Never include URLs / `t.me/...` links / `https://...` in a digest
message.** Just text. The user explicitly does not want links cluttering
the feed; if they want one, they'll ask follow-up ("дай ссылку",
"откуда это"). Then — and only then — you reply with the t.me link to
the specific item.

This rule overrides anything else. If you find yourself adding a link
to a bullet, stop and remove it.

## User interests (filtering rules)

Group output into these four categories. Anything outside them is noise —
drop it without comment.

1. **🇺🇦 Одеса / Україна.** Прильоти й руйнування у місті (з реальною
   шкодою), важливі події Одеси та України загалом, зміни для ФОП
   (податки, ставки, ЄСВ, звітність), нові закони / постанови про
   ухилянтів і мобілізацію які реально зачіпають мешканця, великі
   кадрові / законодавчі зрушення, інфраструктурні обмеження
   (відключення світла, транспорт, перекриття).
   
   **Skip aggressively:** одиничні побутові інциденти (вбивства,
   нещасні випадки, дрібні крадіжки), культурно-історичні замітки,
   археологія, святкові церемонії-рутина, "нардеп предложил…" без
   просування, суперечки фракцій без наслідків, weather, "ТЦК схопили
   когось" як одинична подія (тренди / нові правила — так, окремі
   випадки — ні), курйози, tabloid.

2. **🇲🇩 ПМР / Молдова.** Кратко. Только реально значимое: вступление
   ПМР в Молдову (любые шаги в эту сторону), движение Молдовы в ЕС
   (статус кандидата, открытие переговорных кластеров, дорожные карты),
   военная активность вокруг ПМР, существенные экономические сдвиги
   которые ощутит житель региона. **Все остальное — выкидываем
   агрессивно.** Если за 24h ничего реального не произошло, секцию не
   включаем вообще.

3. **⚔️ Конфликт РФ-Украина.** Только значимые события: крупные
   переговоры, мирные инициативы, существенные изменения на фронте,
   удары по критической инфраструктуре, новые типы вооружений в бою,
   санкции с реальным эффектом, важные заявления Зеленского / Путина /
   Трампа. Skip: тактические бои за деревню, бытовая пропаганда, "ВСУ
   уничтожили N техники" без контекста, "источники сообщают…" без
   подтверждения.

4. **🌍 Мир.** Действительно значимое для глобальной картины:
   - заявления и решения мировых лидеров с последствиями (Трамп, Си,
     Шольц/Мерц/etc, лидеры G7/G20), не рутина
   - крупные геополитические события (войны, санкции, торговые войны)
   - вирусы / эпидемии / биоугрозы с реальным потенциалом
   - прорывные технологии (квантовые вычисления, биотех, новая физика —
     не про релизы LLM, для этого есть tech-digest)
   - стихийные бедствия мирового масштаба
   
   Skip: спорт, развлечения, селебрити, "ученые сказали", корпоративные
   новости среднего масштаба.

## Scope — three modes

The digest can run in one of three scopes depending on the request.

### Mode 1: Full digest (default)

Triggers: cron tick, generic "что нового / новости / что важного / расскажи
что произошло" without any qualifier.

Output: ALL four predefined categories below, skipping categories with
nothing in the window.

### Mode 2: Predefined-category-only

Triggers: the user names one of the four pre-defined categories.

Mapping:

| User says | Output ONLY |
|---|---|
| "что в Одессе", "новости Одессы", "что у нас", "что в Украине", "по Украине" | 🇺🇦 Одесса / Украина |
| "что в ПМР", "Тирасполь", "Молдова", "по Молдове" | 🇲🇩 ПМР / Молдова |
| "что по войне", "по фронту", "что у Зеленского/Путина", "по конфликту", "по СВО" | ⚔️ Конфликт РФ-Украина |
| "что в мире", "мировые новости", "на западе", "по миру" | 🌍 Мир |

Output: exactly one heading + its bullets. Don't add "а ещё в мире..."
unless the user follows up. Header on top: `📰 Новости · <D месяца>`.

### Mode 3: Ad-hoc topic

Triggers: the user asks about a specific subject that does NOT fit any
of the four predefined categories. Examples:

- "что там с ТЦК" → topic = ТЦК / мобилизация
- "что про Газу" → topic = Газа / Израиль-ХАМАС
- "что говорит Маск" → topic = Илон Маск
- "как там с курсом гривны" → topic = курс гривны
- "что нового по тарифам ЕС" → topic = тарифы ЕС
- "что там по AfD в Германии" → topic = AfD / Германия

Output: single-section digest **focused only on that topic**, regardless
of which predefined category the underlying posts would normally fall
into. Apply ALL the same rules — significance bar, consolidation,
Russian-only, no links by default. Format:

```
📰 Новости · <D месяца> · <topic>

• <event>
• <event>
```

The topic line in the header should match how the user named it (or the
canonical Russian form if they used a foreign-language label). Pick a
fitting emoji for the topic if there's an obvious one (🪖 для ТЦК,
🇮🇱 для Израиля, 💰 для курсов, etc); otherwise just the topic name.

If after filtering you have **no matching posts** in the 24h window,
reply briefly: "По теме «<topic>» за последние сутки ничего значимого".
Don't pad with adjacent topics.

### Disambiguation

If the request is ambiguous between Mode 2 and Mode 3 (e.g. "что в
Украине по ФОП" — both Ukraine-category and ФОП-specific), prefer the
narrower Mode 3 ad-hoc topic. The user named a specific thing; respect
it.

If genuinely unclear ("новости" alone), default to Mode 1.

## Protocol

1. **Pull recent digest history (anti-duplication).** Before composing
   anything, fetch the last ~30 messages from the Telegram chat:

   ```
   get_telegram_chat_history(chatId=<TELEGRAM_DEFAULT_CHAT_ID from env>, limit=30)
   ```

   Scan the assistant-role messages for previous digests (any message
   starting with `📰 Новости`, `📰 Каналы`, `🧠 IT-дайджест`). Extract
   the events / bullets that were already covered. **Don't re-send any
   event that already appeared in a recent digest.** This is critical:
   a story that appeared yesterday is not new today, even if channels
   are still re-posting it.

   For ad-hoc topic requests (Mode 3), you also need this step to avoid
   repeating bullets if you already answered a similar topical query
   earlier today.

2. **Read harvested channel posts.** The userbot poller has already
   collected posts from every subscribed channel into the local store —
   you don't need to discover channels or do per-channel fetches. The
   read watermark from the previous digest is **already in your system
   prompt** under `Current context` → `News last read at`. Do NOT call
   a separate tool to fetch it.

   Query the store starting from that point:

   ```
   list_channel_posts(since=<News last read at from context>)
   ```

   If the context says `never (bootstrap with now - 24h)`, use `now - 24h`
   as `since`. Posts come back across all channels chronologically, each
   with `chat_title`, `chat_username`, `posted_at`, `text`, `views`,
   `forwards`. For an ad-hoc one-channel question (Mode 3), pass
   `channel="<username or chat_id>"`.

   Stale-data note: the userbot poller refreshes every ~30min, so posts
   published in the last few minutes may not be there yet. That's fine
   for daily digests; if the user clearly wants something "right now",
   say so.

3. **Drop already-covered events.** Cross-reference each post against
   step 1's extracted history. If the same event / announcement / strike
   / statement was already in a previous digest, drop it. Use semantic
   match, not literal text — "Трамп продлил перемирие" and "Зазначає,
   що перемир'я може бути продовжене" cover the same event.

4. **Score each post against the four categories.** For every remaining
   post, decide:
   - which category (if any) it fits — be strict, prefer skipping over
     stretching
   - whether the post crosses the **significance bar** (see section
     below)
   - whether other posts cover the same or similar event — these must
     be **consolidated**, not listed separately (see section below)

5. **Compose ONE Telegram message.** Plain text, **always in Russian** —
   even if the source post was in Ukrainian, English, or any other
   language, the digest itself goes in Russian. Translate / paraphrase
   the gist, don't copy-paste foreign-language fragments. Format:

   ```
   📰 Новости · <D месяца>

   🇺🇦 Одесса / Украина
   • <event in 1-2 sentences>
   • <next>

   🇲🇩 ПМР / Молдова
   • <event>

   ⚔️ Конфликт РФ-Украина
   • <event>

   🌍 Мир
   • <event>
   ```

   - **Group by topical category, NOT by channel.** The four headings
     above (Одеса/Україна, ПМР/Молдова, Конфликт РФ-Украина, Мир) are
     the only allowed grouping level. Bullets go directly under them.
     Never write headers like "Курс Одесса:", "Типичная Одесса:",
     channel-named subgroups, or any other channel-derived grouping
     inside a category — those are banned.
   - Skip whole categories that have nothing in the window. Don't write
     "ничего важного" — just omit the heading.
   - 1–2 sentences per item, **all bullets in Russian** regardless of
     source language. Use Russian transliteration / translation for
     Ukrainian place names if the natural Russian form exists ("Одесса"
     not "Одеса", "Измаил" not "Ізмаїл", "Львов" not "Львів"); keep
     proper names of organizations / officials in their canonical form.
   - **No links by default.** Don't include `t.me/...` URLs in the
     digest — they're noise on the screen. Internally remember which
     post each bullet came from (you'll need it if asked) but don't
     output it. **Exception:** if the user explicitly asks for a link
     ("дай ссылку на это", "откуда это", "где можно почитать
     подробнее"), reply with the t.me link to the original. Treat
     follow-up "ссылку" / "источник" requests as referring to the
     items you just sent.

6. **Return the composed digest as your final answer.** Emit the
   digest body as your assistant message **with no tool calls** — that
   terminates the loop and hands the text back to whoever invoked you.
   Do NOT call any delivery / notification tool yourself: the caller
   owns delivery. Do NOT stamp the read watermark either — the caller
   decides whether this read should advance the global watermark (a
   different process might be peeking at posts without wanting to
   skip them on the next digest). The text you return is exactly what
   gets forwarded to the user, so make sure it's
   already formatted per the rules above (header, sections, bullets,
   Russian-only, no links).

## Significance bar — the "would I care?" test

Before including any item, ask: **does this affect a typical resident's
plans, decisions, safety, finances, freedom of movement, or
understanding of an unfolding situation that touches them?** If the
answer is "no, this is just trivia / one incident / cultural color /
human-interest", skip it.

These are real items from a previous digest that should NOT have been
included — use them as the reference for what "noise" looks like, even
when the post is in a "news" channel:

- ❌ "Парень с травмой члена после секса с мужчиной (Овидиопольский р-н)"
  — pure tabloid trivia, zero relevance.
- ❌ Единичное бытовое преступление или несчастный случай (убийство,
  ограбление, ДТП с одним пострадавшим и т.п.) — одиночный инцидент не
  новость, если за ним не стоит тренд, угроза общественной безопасности
  или политическая подоплёка.
- ❌ "Победный номер «Красной Звезды» — одесская полоса" — historical
  reference / cultural trivia. Meaningless for *daily* news.
- ❌ "Археологи начали сезонные раскопки в городе" — routine cultural
  activity.
- ❌ "Девушка пыталась остановить похищение (вероятно ТЦК), не получилось"
  — single bystander incident that didn't change anything. **Aggregate
  ТЦК trends or new policies are in scope; one isolated event is not.**
- ❌ "Нардеп Федиенко предлагает не выпускать забронированных за границу"
  — a single deputy's proposal with no traction is just an opinion.
  Becomes news only if it advances (committee vote, draft submitted,
  party support, etc).
- ❌ "В магазине X акция / нардеп Y приехал на завод Z" — local color.
- ❌ "Эксперт сказал…" / "источники сообщают…" — without
  authoritative attribution and concrete claim, skip.

The shared pattern: **one isolated incident that didn't kill many,
didn't shift policy, didn't reveal a trend, didn't change anything →
skip**. Telegram channels publish dozens of such items per day; nearly
all of them are noise.

## Consolidation — merge similar events

When multiple posts (across channels OR across cities) describe the
**same kind of event** or the **same actual event**, merge into ONE
bullet. Don't list 3 nearly-identical items.

Examples:

- 3 cities held Victory Day flower ceremonies → one bullet:
  "В нескольких городах прошли возложения цветов ко Дню Победы
  (Одесса, Измаил, …)". (Internally remember the highest-engagement
  source post in case the user asks for a link.)
- Multiple channels each report the same air-alert / strike / official
  statement → one bullet. (Link only if asked.)
- Multiple ТЦК-related incidents in different cities, none individually
  significant → either merge into one bullet ("ряд инцидентов с ТЦК в
  разных городах") IF the trend itself is the point, or skip entirely
  if the events stay below the significance bar even in aggregate.
- Several channels covering Trump's same statement → one bullet. If
  later asked for a link, prefer a primary source or the channel with
  most coverage.

When in doubt between "consolidate or skip", prefer skip. A clean
4-item digest beats an over-stuffed one with merged filler.

## Rules

- **Ruthless filtering > completeness.** A 4-item digest of real news
  beats a 20-item digest with filler. If the day was quiet, send a short
  "тихий день" message with whatever 1-2 things happened.
- **One message.** If you'd exceed 4000 chars, drop the lowest-priority
  items.
- **Always Russian output.** No matter the source language (Ukrainian,
  English, Romanian, etc) — the digest body is always in Russian.
  Place names: prefer the canonical Russian form ("Одесса", "Измаил",
  "Львов", "Кишинёв"). Don't quote untranslated foreign phrases.
- **Date format in the header**: human-readable Russian, day +
  genitive-case month, no year. Examples: `9 мая`, `28 сентября`,
  `1 января`. Never `2026-05-09` or `09.05.2026` in the digest header.
- **Don't fabricate.** If a post is sparse or unclear, summarize what's
  actually there. Don't infer scale, casualties, or motives that aren't
  stated.
- **No commentary about your own selection.** Just the digest.
- **Don't moralize, don't editorialize.** Neutral reporting style only.
- **Engagement-first** within each category — surface the highest-impact
  / most-covered event first.

## Don'ts

- **Don't include any URLs or t.me links in the digest body.** See the
  hard rule at the top. Links only appear in **follow-up replies** when
  the user explicitly requests them.
- Don't include posts about IT / model releases / framework updates —
  those go through `tech-digest`.
- Don't include posts that are obvious ads, giveaways, or channel
  cross-promotion.
- Don't include "good morning"-style filler, even from news channels.
- Don't summarize channels of type other than `channel`.
- Don't re-send events that already appeared in a recent digest (see
  Protocol step 1 + step 3).
