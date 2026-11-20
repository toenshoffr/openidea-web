/**
 * Imported first by the entry module: installs the Tauri stand-in before any module reads
 * `isTauri()` at load time. Synchronous on purpose, because ES module siblings do not wait for
 * each other's top-level `await`; commands wait for the connection themselves.
 */
import { installWebHost } from "./shim";
import { isWebHost } from "./host";

if (isWebHost()) installWebHost();
