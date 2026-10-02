CREATE TABLE "news_items" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"source" text NOT NULL,
	"external_id" text NOT NULL,
	"title" text,
	"url" text,
	"body" text DEFAULT '' NOT NULL,
	"metadata" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"posted_at" timestamp with time zone,
	"fetched_at" timestamp with time zone DEFAULT now() NOT NULL,
	"embedded_at" timestamp with time zone,
	"embedding" vector(1536),
	CONSTRAINT "news_items_source_external_uniq" UNIQUE("source","external_id")
);
--> statement-breakpoint
CREATE INDEX "news_items_posted_at" ON "news_items" USING btree ("posted_at" DESC NULLS LAST);--> statement-breakpoint
CREATE INDEX "news_items_source_posted" ON "news_items" USING btree ("source","posted_at" DESC NULLS LAST);--> statement-breakpoint
CREATE INDEX "news_items_embedding_ivf" ON "news_items" USING ivfflat ("embedding" vector_cosine_ops) WITH (lists=100);