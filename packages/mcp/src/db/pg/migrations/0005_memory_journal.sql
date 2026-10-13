CREATE TABLE "memory_journal_entries" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"stream_id" bigint NOT NULL,
	"body" text NOT NULL,
	"actor" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "memory_journal_streams" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"capacity" integer NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "memory_journal_streams_name_unique" UNIQUE("name")
);
--> statement-breakpoint
ALTER TABLE "memory_journal_entries" ADD CONSTRAINT "memory_journal_entries_stream_id_memory_journal_streams_id_fk" FOREIGN KEY ("stream_id") REFERENCES "public"."memory_journal_streams"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "memory_journal_entries_stream_id" ON "memory_journal_entries" USING btree ("stream_id","id" DESC NULLS LAST);