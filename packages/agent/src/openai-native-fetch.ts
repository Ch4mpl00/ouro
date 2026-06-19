// Route the openai@4 SDK through the platform's native fetch (undici) instead
// of its bundled node-fetch@2, which throws ERR_STREAM_PREMATURE_CLOSE on
// gzipped responses under Node 22.23+ (every chat/completions call fails).
//
// Import this FIRST in every entry point — before anything pulls in `openai` —
// so the web shim is registered before the SDK auto-detects the node shim.
// Remove once we move to openai@5 (native fetch by default).
import "openai/shims/web";
