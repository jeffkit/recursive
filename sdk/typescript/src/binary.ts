/**
 * Locate the `recursive` CLI binary for subprocess transport.
 */

import { accessSync, constants } from "node:fs";
import { delimiter, join } from "node:path";

import { RecursiveAgentError } from "./exceptions.js";

function isExecutable(path: string): boolean {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Resolve the path to the `recursive` binary.
 *
 * Search order:
 * 1. `override` argument (e.g. `AgentOptions.cliPath`)
 * 2. `RECURSIVE_BIN` environment variable
 * 3. `recursive` on `PATH`
 *
 * Throws {@link RecursiveAgentError} when nothing is found.
 */
export function findRecursiveBinary(override?: string): string {
  if (override) {
    if (!isExecutable(override)) {
      throw new RecursiveAgentError(
        `recursive binary not executable: ${override}`,
      );
    }
    return override;
  }

  const fromEnv = process.env["RECURSIVE_BIN"];
  if (fromEnv) {
    if (!isExecutable(fromEnv)) {
      throw new RecursiveAgentError(
        `RECURSIVE_BIN is set but not executable: ${fromEnv}`,
      );
    }
    return fromEnv;
  }

  const pathEnv = process.env["PATH"] ?? "";
  for (const dir of pathEnv.split(delimiter)) {
    if (!dir) continue;
    const candidate = join(dir, "recursive");
    if (isExecutable(candidate)) return candidate;
  }

  throw new RecursiveAgentError(
    "recursive binary not found. Install the CLI, or set RECURSIVE_BIN / AgentOptions.cliPath.",
  );
}
