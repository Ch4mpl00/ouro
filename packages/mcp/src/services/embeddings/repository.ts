export interface EmbeddableRow {
  id: number;
  source: string;
  externalId: string;
  title: string | null;
  body: string;
  embedding: number[] | null;
}

export interface EmbeddingRepository {
  findByCompositeKeys(targets: { source: string; externalId: string }[]): Promise<EmbeddableRow[]>;
  findByIds(ids: number[]): Promise<EmbeddableRow[]>;
  saveEmbedding(id: number, vector: number[]): Promise<void>;
}
