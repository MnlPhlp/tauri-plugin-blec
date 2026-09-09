// Shared constants and frame helpers for the blec stress GATT protocol.
// See ../../stress-protocol.md for the authoritative description.

export const SERVICE_UUID = "b1ec5747-0000-4000-8000-000000000000";
export const ECHO_UUID = "b1ec5747-0001-4000-8000-000000000000";
export const COUNTER_UUID = "b1ec5747-0002-4000-8000-000000000000";
export const LARGE_UUID = "b1ec5747-0003-4000-8000-000000000000";
export const CONTROL_UUID = "b1ec5747-0004-4000-8000-000000000000";
export const STATUS_UUID = "b1ec5747-0005-4000-8000-000000000000";
export const REPORT_UUID = "b1ec5747-0006-4000-8000-000000000000";

/** All characteristics of the STRESS service, checked by script 6. */
export const ALL_CHARACTERISTICS = [
  ECHO_UUID,
  COUNTER_UUID,
  LARGE_UUID,
  CONTROL_UUID,
  STATUS_UUID,
  REPORT_UUID,
];

export const CTRL_SCRIPT_START = 0x10;
export const CTRL_SCRIPT_ABORT = 0x11;

/** Echo payload size used by the integration scripts ("20 byte write"). */
export const SCRIPT_ECHO_SIZE = 20;

export type ServerStatus = {
  uptime_s: number;
  connects: number;
  drops: number;
  slow_ms: number;
  hidden: boolean;
  fail_ops_left: number;
  echo_seq: number;
};

export type ReportState = "idle" | "running" | "done" | "aborted";

export type ReportStep = {
  name: string;
  ok: boolean | null;
  detail: string;
};

export type Report = {
  script: number;
  name: string;
  state: ReportState;
  elapsed_ms: number;
  steps: ReportStep[];
};

export function u16le(v: number): number[] {
  return [v & 0xff, (v >> 8) & 0xff];
}

export function readU32le(data: number[], offset = 0): number {
  return (
    (data[offset] |
      (data[offset + 1] << 8) |
      (data[offset + 2] << 16) |
      (data[offset + 3] << 24)) >>>
    0
  );
}

export function bytesEqual(a: number[], b: number[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

export function hex(data: number[], max = 16): string {
  const s = data
    .slice(0, max)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join(" ");
  return data.length > max ? `${s} … (${data.length} bytes)` : s;
}

export function decodeUtf8(data: number[]): string {
  return new TextDecoder().decode(new Uint8Array(data));
}

/**
 * Random payload with a marker in the first four bytes
 * (iteration u16 LE, write index u16 LE) so a frame can be matched with a
 * log line on the server side.
 */
export function makePayload(size: number, iteration: number, index: number): number[] {
  const buf = new Uint8Array(Math.max(size, 0));
  crypto.getRandomValues(buf);
  const marker = [...u16le(iteration & 0xffff), ...u16le(index & 0xffff)];
  for (let i = 0; i < marker.length && i < buf.length; i++) buf[i] = marker[i];
  return Array.from(buf);
}
