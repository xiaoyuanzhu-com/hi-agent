// `@hi/core` as a shared chunk. The SessionContext object lives here; the host
// SessionProvider and an agent view's useSpeech() must read the same context
// object, so it must be one instance shared via the import map.
export * from "../core";
