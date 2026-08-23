CREATE TABLE "memory_doc_patches" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"doc_id" bigint NOT NULL,
	"pid" text NOT NULL,
	"kind" text NOT NULL,
	"edits" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"body_before" text DEFAULT '' NOT NULL,
	"version_before" integer NOT NULL,
	"version_after" integer NOT NULL,
	"actor" text NOT NULL,
	"rationale" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "memory_patch_doc_pid_uniq" UNIQUE("doc_id","pid")
);
--> statement-breakpoint
CREATE TABLE "memory_facts" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"body" text NOT NULL,
	"tags" text[] DEFAULT '{}'::text[] NOT NULL,
	"source" text,
	"state" text DEFAULT 'active' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "memory_index" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"source_ref" text NOT NULL,
	"ref" text NOT NULL,
	"text" text NOT NULL,
	"tags" text[] DEFAULT '{}'::text[] NOT NULL,
	"actor" text,
	"state" text DEFAULT 'active' NOT NULL,
	"ts" timestamp with time zone DEFAULT now() NOT NULL,
	"embedded_at" timestamp with time zone,
	"embedding" vector(1536)
);
--> statement-breakpoint
CREATE TABLE "memory_project_docs" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"project_id" bigint NOT NULL,
	"name" text NOT NULL,
	"summary" text,
	"body_md" text DEFAULT '' NOT NULL,
	"version" integer DEFAULT 1 NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "memory_docs_project_name_uniq" UNIQUE("project_id","name")
);
--> statement-breakpoint
CREATE TABLE "memory_projects" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"slug" text NOT NULL,
	"title" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "memory_projects_slug_unique" UNIQUE("slug")
);
--> statement-breakpoint
ALTER TABLE "memory_doc_patches" ADD CONSTRAINT "memory_doc_patches_doc_id_memory_project_docs_id_fk" FOREIGN KEY ("doc_id") REFERENCES "public"."memory_project_docs"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "memory_project_docs" ADD CONSTRAINT "memory_project_docs_project_id_memory_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."memory_projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "memory_patches_doc_created" ON "memory_doc_patches" USING btree ("doc_id","created_at" DESC NULLS LAST);--> statement-breakpoint
CREATE INDEX "memory_facts_tags" ON "memory_facts" USING gin ("tags");--> statement-breakpoint
CREATE INDEX "memory_index_source_ref" ON "memory_index" USING btree ("source_ref");--> statement-breakpoint
CREATE INDEX "memory_index_state_ts" ON "memory_index" USING btree ("state","ts" DESC NULLS LAST);--> statement-breakpoint
CREATE INDEX "memory_index_tags" ON "memory_index" USING gin ("tags");