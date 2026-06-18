CREATE TABLE `improver_state` (
	`skill` text NOT NULL,
	`axis` text NOT NULL,
	`last_attempt_at` text DEFAULT (datetime('now')) NOT NULL,
	`last_outcome` text NOT NULL,
	`shipped_at` text,
	`shipped_lesson` text,
	`baseline_mean` real,
	`baseline_n` real,
	`monitor_status` text,
	PRIMARY KEY(`skill`, `axis`)
);
