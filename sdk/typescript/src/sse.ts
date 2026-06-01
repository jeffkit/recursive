/**
 * Minimal SSE parser for Node.js streams.
 *
 * Reads lines from an async iterable (e.g. a fetch response body) and yields
 * parsed `{ type, data }` event objects.
 */
export interface SseEvent {
  type: string;
  data: unknown;
}

export async function* parseSse(
  lines: AsyncIterable<string>,
): AsyncGenerator<SseEvent> {
  let eventType = "message";
  const dataParts: string[] = [];

  for await (const line of lines) {
    if (line === "") {
      // Blank line = dispatch event
      if (dataParts.length > 0) {
        const payload = dataParts.join("\n");
        let parsed: unknown;
        try {
          parsed = JSON.parse(payload);
        } catch {
          parsed = { raw: payload };
        }
        yield { type: eventType, data: parsed };
      }
      eventType = "message";
      dataParts.length = 0;
      continue;
    }

    if (line.startsWith("event:")) {
      eventType = line.slice(6).trim();
    } else if (line.startsWith("data:")) {
      dataParts.push(line.slice(5).trim());
    }
    // Ignore comment lines (`: ...`)
  }
}

/**
 * Split a Node.js `ReadableStream<Uint8Array>` (from `fetch`) into lines.
 */
export async function* streamToLines(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<string> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const parts = buffer.split("\n");
      buffer = parts.pop() ?? "";
      for (const part of parts) {
        yield part;
      }
    }
    if (buffer) yield buffer;
  } finally {
    reader.releaseLock();
  }
}
