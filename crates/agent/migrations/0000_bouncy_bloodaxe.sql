-- Baseline migration. CREATE … IF NOT EXISTS so Drizzle can adopt a
-- pre-existing agent.db (the prod DB already carries memory/traces/judgements
-- from the raw-schema era): on an existing DB the schema already matches so
-- this is a verified no-op that simply records the migration as applied; on a
-- fresh DB it creates everything. Only this baseline is hand-made idempotent —
-- subsequent generated migrations are ordinary diffs.
CREATE TABLE IF NOT EXISTS `judgements` (
	`trace_id` text NOT NULL,
	`observation_id` text NOT NULL,
	`provider` text NOT NULL,
	`prompt_version` text NOT NULL,
	`node_kind` text NOT NULL,
	`skill` text NOT NULL,
	`query_formulation` real,
	`process` real,
	`coverage` real,
	`composition` real,
	`faithfulness` real,
	`detail` text NOT NULL,
	`created_at` text DEFAULT (datetime('now')) NOT NULL,
	PRIMARY KEY(`trace_id`, `observation_id`, `provider`, `prompt_version`),
	FOREIGN KEY (`trace_id`) REFERENCES `traces`(`id`) ON UPDATE no action ON DELETE no action
);
--> statement-breakpoint
CREATE INDEX IF NOT EXISTS `judgements_skill` ON `judgements` (`skill`,`prompt_version`);--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `memory` (
	`key` text PRIMARY KEY NOT NULL,
	`value` text NOT NULL,
	`updated_at` text DEFAULT (datetime('now')) NOT NULL
);
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `traces` (
	`id` text PRIMARY KEY NOT NULL,
	`name` text NOT NULL,
	`source` text,
	`skill` text,
	`session_id` text,
	`tags` text DEFAULT '[]' NOT NULL,
	`input` text,
	`output` text,
	`metadata` text,
	`observations` text DEFAULT '[]' NOT NULL,
	`started_at` text NOT NULL,
	`created_at` text DEFAULT (datetime('now')) NOT NULL
);
--> statement-breakpoint
CREATE INDEX IF NOT EXISTS `traces_started` ON `traces` (`started_at`);--> statement-breakpoint
CREATE INDEX IF NOT EXISTS `traces_skill` ON `traces` (`skill`);
