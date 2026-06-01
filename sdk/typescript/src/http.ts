/**
 * Internal HTTP client (not part of the public API).
 */

import { RecursiveAgentError } from "./exceptions.js";
import { parseSse, streamToLines, type SseEvent } from "./sse.js";

export interface HttpClientOptions {
  baseUrl: string;
  apiKey?: string;
  timeout?: number;
}

export class HttpClient {
  readonly baseUrl: string;
  private readonly headers: Record<string, string>;

  constructor({ baseUrl, apiKey }: HttpClientOptions) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.headers = {
      "Content-Type": "application/json",
    };
    const key = apiKey ?? process.env["RECURSIVE_API_KEY"];
    if (key) {
      this.headers["x-api-key"] = key;
    }
  }

  async get(path: string): Promise<unknown> {
    const resp = await this._fetch("GET", path);
    return resp.json();
  }

  async post(path: string, body: unknown): Promise<unknown> {
    const resp = await this._fetch("POST", path, body);
    return resp.json();
  }

  async delete(path: string): Promise<void> {
    await this._fetch("DELETE", path);
  }

  async *streamEvents(path: string): AsyncGenerator<SseEvent> {
    let resp: Response;
    try {
      resp = await fetch(`${this.baseUrl}${path}`, {
        method: "GET",
        headers: { ...this.headers, Accept: "text/event-stream" },
      });
    } catch (err) {
      throw new RecursiveAgentError(
        `Cannot reach Recursive server at ${this.baseUrl}: ${err}`,
        { isRetryable: true },
      );
    }

    if (!resp.ok) {
      const text = await resp.text();
      throw new RecursiveAgentError(
        `HTTP ${resp.status}: ${text}`,
        { isRetryable: resp.status >= 500 },
      );
    }

    if (!resp.body) {
      throw new RecursiveAgentError("Response body is null");
    }

    yield* parseSse(streamToLines(resp.body));
  }

  private async _fetch(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<Response> {
    try {
      const resp = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers: this.headers,
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });

      if (!resp.ok) {
        const text = await resp.text();
        throw new RecursiveAgentError(
          `HTTP ${resp.status}: ${text}`,
          { isRetryable: resp.status >= 500 },
        );
      }

      return resp;
    } catch (err) {
      if (err instanceof RecursiveAgentError) throw err;
      throw new RecursiveAgentError(
        `Cannot reach Recursive server at ${this.baseUrl}: ${err}`,
        { isRetryable: true },
      );
    }
  }
}
