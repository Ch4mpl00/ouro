-- IF NOT EXISTS: on a large table the index is built ahead of the deploy with
-- CREATE INDEX CONCURRENTLY (same name), because migrations run in a
-- transaction on boot and a plain build would hold writes for its duration.
CREATE INDEX IF NOT EXISTS "news_items_embedding_hnsw" ON "news_items" USING hnsw (("embedding"::halfvec(1536)) halfvec_cosine_ops);
