"use client";

// Virtual DVR, the recording engine behind the deck: the tenant's OWN browser does the work (the playing <video> is captured with captureStream() +
// MediaRecorder, so no fleet node proxies, transcodes or stores the feed) and the finished recording is uploaded to the project's Drive under `tv/` through the
// ordinary authenticated Drive API, so quota, sharing, WebDAV and deletion work as for any file. A recording has a start (now or a future clock time) and a
// duration and stops itself on time; it does NOT survive a page close, an honest limit of browser-side capture.

import { apiPutBytes } from "@/lib/api";

export type DvrState = "scheduled" | "recording" | "uploading" | "saved" | "failed";

export interface DvrRecording {
  id: string;
  project: string;
  region: string;
  channel: string;
  /** Epoch ms the recording should start (<= now means "already started"). */
  startAt: number;
  /** Requested length in ms. */
  durationMs: number;
  state: DvrState;
  /** Bytes written so far (post-stop for the final size). */
  bytes: number;
  /** Drive path once saved, or the failure reason. */
  path?: string;
  error?: string;
}

const RECORDER_TIMESLICE_MS = 1000;
const MIN_RECORDING_MS = 1000;

const MIME_CANDIDATES = [
  'video/webm;codecs="vp9,opus"',
  'video/webm;codecs="vp8,opus"',
  "video/webm",
  "video/mp4",
];

/** The first container this browser can actually record, or null. */
export function pickMime(): string | null {
  if (typeof MediaRecorder === "undefined") return null;
  for (const m of MIME_CANDIDATES) {
    try {
      if (MediaRecorder.isTypeSupported(m)) return m;
    } catch {}
  }
  return null;
}

/** `tv/<region>/<channel>-<timestamp>.<ext>` — the Drive path a recording lands at. */
export function drivePath(rec: Pick<DvrRecording, "region" | "channel">, mime: string, at: number): string {
  const safe = (s: string) =>
    s
      .toLowerCase()
      .replace(/[^a-z0-9._-]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "channel";
  const d = new Date(at);
  const stamp = [
    d.getFullYear(),
    String(d.getMonth() + 1).padStart(2, "0"),
    String(d.getDate()).padStart(2, "0"),
    "-",
    String(d.getHours()).padStart(2, "0"),
    String(d.getMinutes()).padStart(2, "0"),
  ].join("");
  const ext = mime.startsWith("video/mp4") ? "mp4" : "webm";
  return `tv/${safe(rec.region)}/${safe(rec.channel)}-${stamp}.${ext}`;
}

export interface RecordHandle {
  stop: () => void;
}

/** Captures `video` for `durationMs`, then uploads to the project's Drive; `onUpdate` fires on every state/byte change for a live progress bar, and the
 *  returned handle's `stop()` ends the recording early (the partial recording is still saved). */
export function startRecording(opts: {
  video: HTMLVideoElement;
  rec: DvrRecording;
  onUpdate: (patch: Partial<DvrRecording>) => void;
}): RecordHandle {
  const { video, rec, onUpdate } = opts;
  const mime = pickMime();
  if (!mime) {
    onUpdate({ state: "failed", error: "This browser cannot record video (no MediaRecorder support)." });
    return { stop: () => {} };
  }
  const capture = (video as HTMLVideoElement & { captureStream?: () => MediaStream; mozCaptureStream?: () => MediaStream });
  const stream = capture.captureStream?.() ?? capture.mozCaptureStream?.();
  if (!stream) {
    onUpdate({ state: "failed", error: "This browser cannot capture the video element." });
    return { stop: () => {} };
  }

  let recorder: MediaRecorder;
  try {
    recorder = new MediaRecorder(stream, { mimeType: mime });
  } catch (e) {
    onUpdate({ state: "failed", error: `Recorder rejected the stream: ${String(e)}` });
    return { stop: () => {} };
  }

  const chunks: BlobPart[] = [];
  let bytes = 0;
  const startedAt = Date.now();

  recorder.ondataavailable = (ev) => {
    if (ev.data && ev.data.size > 0) {
      chunks.push(ev.data);
      bytes += ev.data.size;
      onUpdate({ bytes });
    }
  };

  recorder.onstop = async () => {
    onUpdate({ state: "uploading", bytes });
    try {
      const blob = new Blob(chunks, { type: mime });
      const path = drivePath(rec, mime, startedAt);
      await apiPutBytes(
        `/v1/drive/${encodeURIComponent(rec.project)}/file?path=${encodeURIComponent(path)}`,
        blob,
        mime,
      );
      onUpdate({ state: "saved", path, bytes: blob.size });
    } catch (e) {
      onUpdate({ state: "failed", error: String(e instanceof Error ? e.message : e) });
    }
  };

  recorder.start(RECORDER_TIMESLICE_MS);
  onUpdate({ state: "recording", bytes: 0 });

  const timer = window.setTimeout(() => {
    if (recorder.state !== "inactive") recorder.stop();
  }, Math.max(MIN_RECORDING_MS, rec.durationMs));

  return {
    stop: () => {
      window.clearTimeout(timer);
      if (recorder.state !== "inactive") recorder.stop();
    },
  };
}

/** Human "1:04:00" / "22:30" for the deck's readouts. */
export function fmtClock(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`
    : `${m}:${String(sec).padStart(2, "0")}`;
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}
