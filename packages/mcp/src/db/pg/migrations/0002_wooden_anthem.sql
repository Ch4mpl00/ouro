CREATE TABLE "knowledge_base_notes" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"body" text NOT NULL,
	"tags" text[] DEFAULT '{}'::text[] NOT NULL,
	"source" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	"embedded_at" timestamp with time zone,
	"embedding" vector(1536)
);
--> statement-breakpoint
CREATE INDEX "kb_notes_created_at" ON "knowledge_base_notes" USING btree ("created_at" DESC NULLS LAST);--> statement-breakpoint
CREATE INDEX "kb_notes_tags" ON "knowledge_base_notes" USING gin ("tags");