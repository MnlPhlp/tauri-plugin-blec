import { reactive } from "vue";

export type LogLevel = "info" | "warn" | "error";

export type LogEntry = {
  ts: number;
  level: LogLevel;
  msg: string;
};

const MAX_ENTRIES = 500;

/**
 * Reactive ring buffer of log entries. Oldest entries are dropped once the
 * buffer holds MAX_ENTRIES lines. Entries are stored oldest-first; the UI
 * renders them reversed.
 */
export const entries = reactive<LogEntry[]>([]);

function push(level: LogLevel, msg: string) {
  const entry: LogEntry = { ts: Date.now(), level, msg };
  entries.push(entry);
  if (entries.length > MAX_ENTRIES) {
    entries.splice(0, entries.length - MAX_ENTRIES);
  }
  const line = `${entry.ts} | ${level} | ${msg}`;
  if (level === "error") console.error(line);
  else if (level === "warn") console.warn(line);
  else console.log(line);
}

export const log = {
  info: (msg: string) => push("info", msg),
  warn: (msg: string) => push("warn", msg),
  error: (msg: string) => push("error", msg),
  clear: () => entries.splice(0, entries.length),
  /**
   * Render the buffer as text, one line per entry:
   * `<unix_ms> | <level> | <msg>` (same timestamp style as the stress-server log,
   * so both logs can be merged and sorted by the first column).
   */
  toText: (): string =>
    entries.map((e) => `${e.ts} | ${e.level} | ${e.msg}`).join("\n"),
};
