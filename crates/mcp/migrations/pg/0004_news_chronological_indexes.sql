-- Bare DESC orders nulls first; the reverse scan also matches ASC NULLS LAST.
-- Keep the historical indexes so existing NULLS LAST queries remain covered.
CREATE INDEX "news_items_posted_at_order" ON "news_items" USING btree ("posted_at" DESC NULLS FIRST);
--> statement-breakpoint
CREATE INDEX "news_items_source_posted_order" ON "news_items" USING btree ("source", "posted_at" DESC NULLS FIRST);
