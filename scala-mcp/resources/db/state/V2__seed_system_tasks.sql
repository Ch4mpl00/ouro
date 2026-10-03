-- The system scheduled tasks, once per fresh database. Guarded by a settings
-- flag rather than by row presence: a system task the user cancelled must
-- stay cancelled. (A database the Rust server created is baselined past this
-- file: it already seeded under the same flag.)
INSERT INTO scheduled_tasks (cron_expr, recurring, prompt, source)
SELECT v.cron_expr, true, v.prompt, v.source
  FROM (VALUES
    ('0 9 * * *', 'Daily news-digest tick. Read posts from the user''s subscribed Telegram channels since the watermark in your session context, filter to the four predefined categories (Одеса/Україна, ПМР/Молдова, Конфликт РФ-Украина, Мир), and post a topical digest to Telegram.', NULL),
    ('0 8 * * *', 'Daily tech-digest tick. Compose a personalized IT news digest for the user (Hacker News, Habr) and post to Telegram. Query the news store via search_news with topics matching the interests in the system prompt — the pollers keep it fresh, no need to fetch articles.', NULL),
    ('0 4 * * *', 'Daily dreaming tick. Review the signals processed since the previous dreaming fire (see ''Previous fire'' header above) and consider whether any skill files deserve an edit based on patterns, recurring user feedback, or failure modes you observed. Use list_signals(since=<previous fire>) to scope the review. Edit skills via write_skill when warranted.', 'dreaming')
  ) AS v (cron_expr, prompt, source)
 WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = 'system.seeded_default_tasks');

INSERT INTO settings (key, value) VALUES ('system.seeded_default_tasks', '1') ON CONFLICT (key) DO NOTHING;
