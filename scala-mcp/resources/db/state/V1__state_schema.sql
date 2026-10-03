-- OAuth / session credentials per integration: Gmail tokens (expires_at is
-- an ISO string, as googleapis wrote it), the userbot's MTProto session.
CREATE TABLE integration_account (
  provider      text NOT NULL,
  account_key   text NOT NULL,
  access_token  text,
  refresh_token text,
  expires_at    text,
  metadata      text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (provider, account_key)
);

-- Every message the bot sees or sends: role 'user' incoming, 'assistant'
-- outgoing. thread_id is the forum topic (NULL for non-topic / General).
CREATE TABLE telegram_messages (
  id            bigserial PRIMARY KEY,
  chat_id       bigint NOT NULL,
  tg_message_id bigint,
  thread_id     bigint,
  role          text NOT NULL CHECK (role IN ('user', 'assistant')),
  text          text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX telegram_messages_chat_id_id ON telegram_messages (chat_id, id);
CREATE INDEX telegram_messages_chat_thread_id ON telegram_messages (chat_id, thread_id, id);

-- Poller cursors: Telegram's last_update_id, Gmail's per-subscription
-- watermarks.
CREATE TABLE telegram_kv (key text PRIMARY KEY, value text NOT NULL);
CREATE TABLE gmail_kv (key text PRIMARY KEY, value text NOT NULL);

-- User-facing settings: `timezone` (IANA), the system-task seed flag.
CREATE TABLE settings (
  key        text PRIMARY KEY,
  value      text NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Cron-driven tasks in the user's timezone. One-shots retire once
-- last_run_at (unix seconds of the slot fired for) is set; cancel = DELETE.
-- source NULL fires as `scheduler`.
CREATE TABLE scheduled_tasks (
  id          bigserial PRIMARY KEY,
  cron_expr   text NOT NULL,
  recurring   boolean NOT NULL,
  prompt      text NOT NULL,
  source      text,
  last_run_at bigint,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX scheduled_tasks_pending ON scheduled_tasks (recurring, last_run_at);

-- The signal queue: pollers enqueue, the agent pops via get_next_signal.
CREATE TABLE signals (
  id          bigserial PRIMARY KEY,
  source      text NOT NULL,
  content     text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  consumed_at timestamptz
);
CREATE INDEX signals_pending ON signals (id) WHERE consumed_at IS NULL;
