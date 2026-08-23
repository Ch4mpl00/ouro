export { createMemoryModule, type MemoryModule, type MemoryModuleDeps } from "./module";
export { createMemoryService, MemoryError, type MemoryService, type RecallHit } from "./service";
export { createIndexer, type Indexer } from "./indexer";
export { createPgMemoryStore } from "./store.pg";
export { createInMemoryStore } from "./store.memory";
export type { MemoryStore } from "./store";
export {
  importLegacyNotes,
  legacyNoteSource,
  type ImportNotesResult,
  type LegacyNote,
} from "./import-notes";
export { parseRef, docRef, factRef, type MemoryRef } from "./refs";
export type { Doc, DocSummary, Fact, MemoryState, Project } from "./types";
