/**
 * Thrown when the agent run could **not start** — auth failure, network error,
 * bad configuration, etc.
 *
 * This is distinct from a run that started but failed (`RunResult.status === "error"`).
 */
export class RecursiveAgentError extends Error {
  readonly isRetryable: boolean;

  constructor(message: string, options?: { isRetryable?: boolean }) {
    super(message);
    this.name = "RecursiveAgentError";
    this.isRetryable = options?.isRetryable ?? false;
    // Maintain proper prototype chain in transpiled JS
    Object.setPrototypeOf(this, new.target.prototype);
  }
}
