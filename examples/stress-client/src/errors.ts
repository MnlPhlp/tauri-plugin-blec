/** Thrown between steps once stop() was requested. */
export class StopError extends Error {
  constructor() {
    super("stopped");
    this.name = "StopError";
  }
}

/** Thrown by the op() wrapper after the error has been counted and logged. */
export class OpError extends Error {
  constructor(
    public op: string,
    public detail: string,
    public timeout: boolean
  ) {
    super(`${op}: ${detail}`);
    this.name = "OpError";
  }
}

export function errStr(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}
