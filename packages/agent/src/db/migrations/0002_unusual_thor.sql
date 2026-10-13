CREATE TABLE `activity_projections` (
	`trace_id` text PRIMARY KEY NOT NULL,
	`status` text NOT NULL,
	`attempts` integer DEFAULT 0 NOT NULL,
	`next_attempt_at` text,
	`error` text,
	`draft` text,
	`entry_id` integer,
	`updated_at` text DEFAULT (datetime('now')) NOT NULL,
	FOREIGN KEY (`trace_id`) REFERENCES `traces`(`id`) ON UPDATE no action ON DELETE no action
);
--> statement-breakpoint
CREATE INDEX `activity_projections_status` ON `activity_projections` (`status`,`next_attempt_at`);--> statement-breakpoint
CREATE INDEX `traces_created` ON `traces` (`created_at`);